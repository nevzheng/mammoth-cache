//! The REAPI cache over gRPC (design doc 0016): `Capabilities`, `ActionCache`,
//! `ContentAddressableStorage` and `ByteStream`, on their own HTTP/2 listener. `Execution` is not
//! served: mammoth-cache is a cache, not a remote build system.
//!
//! Entries share the Bazel HTTP cache's namespaces and keys (design doc 0012), so a blob written
//! over one protocol is a hit over the other. Clients point at it with, for Bazel,
//! `--remote_cache=grpc://<host>:6011` (or `grpcs://` with `[tls]`).

mod auth;
mod digest;
mod observe;
pub mod proto;
mod services;
mod status;

use std::{future::Future, io, sync::Arc};

use axum::{
    extract::{Request, State},
    middleware::{self, Next},
    response::Response,
};
use tokio::{net::TcpListener, sync::Semaphore};
use tonic::service::Routes;

pub use services::{MAX_MESSAGE_BYTES, MAX_REQUEST_BYTES, Services};

use crate::{
    api::error::Reason,
    auth::Authenticator,
    cache::Cache,
    server::{self, ServerSettings, TlsSettings},
};
use proto::{
    build::bazel::remote::execution::v2::{
        action_cache_server::ActionCacheServer, capabilities_server::CapabilitiesServer,
        content_addressable_storage_server::ContentAddressableStorageServer,
    },
    google::bytestream::byte_stream_server::ByteStreamServer,
};

/// Calls that buffer whole messages (every method but `ByteStream`'s) running at once on the node.
/// Each can hold a request of up to [`MAX_REQUEST_BYTES`], a response it assembles, and that
/// response encoded, so this bounds the node's gRPC buffers at about 512 MiB. More calls wait.
const MAX_BUFFERED_CALLS: usize = 32;

/// The services over `cache`, each call checked against `auth`. Uploads fail after
/// `body_read_timeout` without data, and `ByteStream.Read` after `send_timeout` without the client
/// taking any.
pub fn routes(cache: Arc<Cache>, auth: Arc<Authenticator>, settings: ServerSettings) -> Routes {
    let services = Services::new(cache, settings.body_read_timeout, settings.send_timeout);
    let routes = Routes::new(CapabilitiesServer::new(services.clone()))
        .add_service(
            ActionCacheServer::new(services.clone())
                .max_decoding_message_size(MAX_REQUEST_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
        .add_service(
            ContentAddressableStorageServer::new(services.clone())
                .max_decoding_message_size(MAX_REQUEST_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
        .add_service(
            ByteStreamServer::new(services)
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        );
    let buffered = Arc::new(Semaphore::new(MAX_BUFFERED_CALLS));
    // The last layer runs first: log and count every call, then authenticate it, then wait for
    // room.
    Routes::from(
        routes
            .into_axum_router()
            .layer(middleware::from_fn(too_large))
            .layer(middleware::from_fn_with_state(buffered, limit))
            .layer(middleware::from_fn_with_state(auth, auth::require))
            .layer(middleware::from_fn(observe::observe))
            .layer(middleware::from_fn(crate::access::grpc)),
    )
}

/// Middleware that answers a message past the decode limit with `RESOURCE_EXHAUSTED`, the gRPC
/// convention for a message too large to receive, where tonic answers `OUT_OF_RANGE`, which
/// clients take to mean a read past the end of a blob.
async fn too_large(request: Request, next: Next) -> Response {
    let response = next.run(request).await;
    let Some(status) = tonic::Status::from_header_map(response.headers()) else {
        return response;
    };
    if status.code() != tonic::Code::OutOfRange
        || !status
            .message()
            .contains("decoded message length too large")
    {
        return response;
    }
    status::error(
        Reason::MessageTooLarge,
        status.message().replace("Error, decoded", "The"),
    )
    .into_http::<axum::body::Body>()
}

/// Middleware that holds one of the node's buffered-call permits while a call other than a
/// `ByteStream` one decodes its request and builds its response. `ByteStream` calls stream in
/// chunks, so they buffer little and may run long.
async fn limit(State(permits): State<Arc<Semaphore>>, request: Request, next: Next) -> Response {
    if request
        .uri()
        .path()
        .starts_with("/google.bytestream.ByteStream/")
    {
        return next.run(request).await;
    }
    // The semaphore is never closed, so this always holds a permit.
    let _permit = permits.acquire().await;
    next.run(request).await
}

/// Serves `routes` over HTTP/2 on `listener`, over TLS when `tls` is set, until `signal`
/// resolves. The connection timeouts and the drain are the HTTP server's (see
/// [`server::serve`]); the caller flushes bucket uploads once both servers have drained.
pub async fn serve(
    listener: TcpListener,
    tls: Option<TlsSettings>,
    routes: Routes,
    settings: ServerSettings,
    signal: impl Future<Output = ()> + Send,
) -> io::Result<()> {
    // The HTTP listener offers only HTTP/1.1; gRPC needs HTTP/2. The certificate resolver is
    // shared, so a reloaded certificate reaches both.
    let tls = tls.map(|tls| {
        let mut config = (*tls.config).clone();
        config.alpn_protocols = vec![b"h2".to_vec()];
        TlsSettings {
            config: Arc::new(config),
            handshake_timeout: tls.handshake_timeout,
        }
    });
    server::serve_http2(listener, tls, routes.into_axum_router(), settings, signal).await
}
