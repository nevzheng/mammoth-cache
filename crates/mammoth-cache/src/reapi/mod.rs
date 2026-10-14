//! The REAPI cache over gRPC (design doc 0016): `Capabilities`, `ActionCache`,
//! `ContentAddressableStorage` and `ByteStream`, on their own HTTP/2 listener. `Execution` is not
//! served: mammoth-cache is a cache, not a remote build system.
//!
//! Entries share the Bazel HTTP cache's namespaces and keys (design doc 0012), so a blob written
//! over one protocol is a hit over the other. Clients point at it with, for Bazel,
//! `--remote_cache=grpc://<host>:6011` (or `grpcs://` with `[tls]`).

mod auth;
mod digest;
pub mod proto;
mod services;
mod status;

use std::{future::Future, io, sync::Arc, time::Duration};

use axum::middleware;
use tokio::net::TcpListener;
use tonic::service::Routes;

pub use services::{MAX_MESSAGE_BYTES, Services};

use crate::{
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

/// The services over `cache`, each call checked against `auth`.
pub fn routes(cache: Arc<Cache>, auth: Arc<Authenticator>, idle_timeout: Duration) -> Routes {
    let services = Services::new(cache, idle_timeout);
    let routes = Routes::new(CapabilitiesServer::new(services.clone()))
        .add_service(
            ActionCacheServer::new(services.clone())
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
        .add_service(
            ContentAddressableStorageServer::new(services.clone())
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
        .add_service(
            ByteStreamServer::new(services)
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        );
    Routes::from(
        routes
            .into_axum_router()
            .layer(middleware::from_fn_with_state(auth, auth::require)),
    )
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
