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

use std::{future::Future, io, pin::pin, sync::Arc, time::Duration};

use axum::middleware;
use futures_util::{FutureExt, StreamExt, stream};
use tokio::{net::TcpListener, sync::mpsc};
use tokio_rustls::TlsAcceptor;
use tonic::{service::Routes, transport::Server};

pub use services::{MAX_MESSAGE_BYTES, Services};

use crate::{auth::Authenticator, cache::Cache, server::TlsSettings};
use proto::{
    build::bazel::remote::execution::v2::{
        action_cache_server::ActionCacheServer, capabilities_server::CapabilitiesServer,
        content_addressable_storage_server::ContentAddressableStorageServer,
    },
    google::bytestream::byte_stream_server::ByteStreamServer,
};

/// Connection settings for [`serve`], from `[server]` in `mammoth.toml`.
#[derive(Debug, Clone, Copy)]
pub struct GrpcSettings {
    /// After the shutdown signal, how long to keep accepting connections while load balancers
    /// notice that `/readyz` fails.
    pub shutdown_delay: Duration,
    /// From the shutdown signal, how long calls in flight may take to finish.
    pub shutdown_timeout: Duration,
    /// How long an upload may send nothing before it fails.
    pub idle_timeout: Duration,
}

/// HTTP/2 pings that find dead peers on idle connections.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// Calls one connection may have open at once.
const MAX_CONCURRENT_STREAMS: u32 = 512;

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
/// resolves. Then, like the HTTP server: keep accepting for `shutdown_delay`, send `GOAWAY`, and
/// let calls in flight finish until `shutdown_timeout` after the signal.
pub async fn serve(
    listener: TcpListener,
    tls: Option<TlsSettings>,
    routes: Routes,
    settings: GrpcSettings,
    signal: impl Future<Output = ()> + Send,
) -> io::Result<()> {
    let (signalled_tx, signalled_rx) = tokio::sync::oneshot::channel::<()>();
    let shutdown = async move {
        signal.await;
        let _ = signalled_tx.send(());
        tokio::time::sleep(settings.shutdown_delay).await;
        tracing::info!("gRPC listener closing; finishing calls in flight");
    };
    let deadline = async move {
        if signalled_rx.await.is_ok() {
            tokio::time::sleep(settings.shutdown_timeout).await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    let mut builder = Server::builder()
        .tcp_nodelay(true)
        .http2_keepalive_interval(Some(KEEPALIVE_INTERVAL))
        .http2_keepalive_timeout(Some(KEEPALIVE_TIMEOUT))
        .max_concurrent_streams(MAX_CONCURRENT_STREAMS);
    let grpc = builder.add_routes(routes);
    let served = match tls {
        None => {
            let incoming = accepted(listener);
            grpc.serve_with_incoming_shutdown(incoming, shutdown)
                .boxed()
        }
        Some(tls) => {
            let incoming = tls_incoming(listener, &tls);
            grpc.serve_with_incoming_shutdown(incoming, shutdown)
                .boxed()
        }
    };
    let mut deadline = pin!(deadline);
    tokio::select! {
        result = served => result.map_err(io::Error::other),
        () = &mut deadline => {
            tracing::warn!("shutdown timeout reached with gRPC calls in flight; abandoning them");
            Ok(())
        }
    }
}

/// How long the accept loop pauses after an error such as running out of file descriptors, as on
/// the HTTP port. Without the pause the listener stays readable and the loop spins.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Accepted connections with `TCP_NODELAY` set. Accept errors are logged once when they start and
/// once when they stop, and pause the loop rather than end the stream.
fn accepted(
    listener: TcpListener,
) -> impl futures_util::Stream<Item = io::Result<tokio::net::TcpStream>> + Send + use<> {
    stream::unfold((listener, false), |(listener, mut failing)| async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    if failing {
                        failing = false;
                        tracing::info!("accepting gRPC connections again");
                    }
                    if let Err(error) = stream.set_nodelay(true) {
                        tracing::debug!(%error, "could not set TCP_NODELAY");
                    }
                    return Some((Ok(stream), (listener, failing)));
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset
                    ) =>
                {
                    tracing::debug!(%error, "connection closed before it was accepted");
                }
                Err(error) => {
                    if !failing {
                        failing = true;
                        tracing::warn!(%error, "cannot accept gRPC connections; retrying every 100ms");
                    }
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                }
            }
        }
    })
}

/// Accepted connections after their TLS handshake. Each handshake runs in its own task, within
/// the handshake timeout, so a slow client cannot hold up the others.
fn tls_incoming(
    listener: TcpListener,
    tls: &TlsSettings,
) -> impl futures_util::Stream<Item = io::Result<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>>
+ use<> {
    // The HTTP listener offers only HTTP/1.1; gRPC needs HTTP/2. The certificate resolver is
    // shared, so a reloaded certificate reaches both.
    let mut config = (*tls.config).clone();
    config.alpn_protocols = vec![b"h2".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let handshake_timeout = tls.handshake_timeout;
    let (tx, rx) = mpsc::channel(64);
    let accept = tokio::spawn(async move {
        let mut connections = pin!(accepted(listener));
        while let Some(Ok(stream)) = connections.next().await {
            if tx.is_closed() {
                break;
            }
            let acceptor = acceptor.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let reason =
                    match tokio::time::timeout(handshake_timeout, acceptor.accept(stream)).await {
                        Ok(Ok(stream)) => {
                            let _ = tx.send(Ok(stream)).await;
                            return;
                        }
                        Ok(Err(error)) => {
                            tracing::debug!(%error, "TLS handshake failed");
                            "error"
                        }
                        Err(_) => {
                            tracing::debug!("TLS handshake timed out");
                            "timeout"
                        }
                    };
                metrics::counter!("mammoth_tls_handshake_failures_total", "reason" => reason)
                    .increment(1);
            });
        }
    });
    // Stops accepting once the server stops reading.
    let guard = AbortOnDrop(accept);
    stream::unfold((rx, guard), |(mut rx, guard)| async move {
        rx.recv().await.map(|item| (item, (rx, guard)))
    })
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
