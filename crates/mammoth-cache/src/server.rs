//! The HTTP/1.1 server: accepts connections, applies timeouts, and drains on shutdown (design doc
//! 0013).
//!
//! `axum::serve` sets no timeouts and waits for open connections without limit, so this module
//! serves each connection with hyper directly.

use std::{future::Future, io, pin::pin, sync::Arc, time::Duration};

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    middleware::{self, Next},
    response::Response,
};
use futures_util::{StreamExt, stream};
use hyper::server::conn::http1;
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    server::graceful::{GracefulShutdown, Watcher},
    service::TowerToHyperService,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream},
    time::Instant,
};
use tokio_rustls::{TlsAcceptor, rustls::ServerConfig};

use crate::cache::Cache;

/// Connection timeouts and shutdown timing, from `[server]` in `mammoth.toml`.
#[derive(Debug, Clone, Copy)]
pub struct ServerSettings {
    /// After the shutdown signal, how long to keep serving while load balancers see `/readyz`
    /// fail.
    pub shutdown_delay: Duration,
    /// From the shutdown signal, how long requests and queued bucket uploads may take to finish.
    pub shutdown_timeout: Duration,
    /// How long a connection may wait for, and take to send, its next request's headers. hyper
    /// starts its header timer when a keep-alive connection goes idle, so one timeout covers both.
    pub idle_timeout: Duration,
    /// How long a request body may send nothing before the request fails.
    pub body_read_timeout: Duration,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            shutdown_delay: Duration::from_secs(5),
            shutdown_timeout: Duration::from_secs(25),
            idle_timeout: Duration::from_secs(75),
            body_read_timeout: Duration::from_secs(60),
        }
    }
}

/// TLS for [`serve_tls`] (design doc 0014).
#[derive(Debug, Clone)]
pub struct TlsSettings {
    /// The rustls config, usually from [`crate::tls::Certificates::server_config`].
    pub config: Arc<ServerConfig>,
    /// How long a client may take to complete the handshake.
    pub handshake_timeout: Duration,
}

/// Serves `app` over plain HTTP on `listener` until `signal` resolves, then drains:
///
/// 1. `/readyz` starts failing, through [`Cache::begin_shutdown`].
/// 2. New connections are still accepted for `shutdown_delay`, while load balancers notice.
/// 3. The listener closes, idle connections close, and requests in flight finish.
/// 4. Queued bucket uploads finish.
///
/// Steps 2 to 4 share the `shutdown_timeout` deadline; whatever is left then is abandoned.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    cache: Arc<Cache>,
    settings: ServerSettings,
    signal: impl Future<Output = ()>,
) -> io::Result<()> {
    run(listener, None, app, cache, settings, signal).await
}

/// Like [`serve`], but over TLS: each connection completes a handshake, within
/// `handshake_timeout`, before hyper serves it. Timeouts and the drain are the same.
pub async fn serve_tls(
    listener: TcpListener,
    tls: TlsSettings,
    app: Router,
    cache: Arc<Cache>,
    settings: ServerSettings,
    signal: impl Future<Output = ()>,
) -> io::Result<()> {
    let tls = Tls {
        acceptor: TlsAcceptor::from(tls.config),
        handshake_timeout: tls.handshake_timeout,
    };
    run(listener, Some(tls), app, cache, settings, signal).await
}

struct Tls {
    acceptor: TlsAcceptor,
    handshake_timeout: Duration,
}

async fn run(
    listener: TcpListener,
    tls: Option<Tls>,
    app: Router,
    cache: Arc<Cache>,
    settings: ServerSettings,
    signal: impl Future<Output = ()>,
) -> io::Result<()> {
    let app = app.layer(middleware::from_fn_with_state(
        settings.body_read_timeout,
        body_read_timeout,
    ));
    let graceful = GracefulShutdown::new();
    let mut signal = pin!(signal);
    loop {
        tokio::select! {
            accepted = listener.accept() => accept(accepted, &app, &graceful, settings, tls.as_ref()),
            () = &mut signal => break,
        }
    }

    let deadline = Instant::now() + settings.shutdown_timeout;
    cache.begin_shutdown();
    tracing::info!(
        delay = ?settings.shutdown_delay,
        timeout = ?settings.shutdown_timeout,
        "shutting down: /readyz now fails"
    );
    let stop_accepting = tokio::time::sleep(settings.shutdown_delay);
    let mut stop_accepting = pin!(stop_accepting);
    loop {
        tokio::select! {
            accepted = listener.accept() => accept(accepted, &app, &graceful, settings, tls.as_ref()),
            () = &mut stop_accepting => break,
        }
    }
    drop(listener);

    tracing::info!("no longer accepting connections; finishing requests in flight");
    if tokio::time::timeout_at(deadline, graceful.shutdown())
        .await
        .is_err()
    {
        tracing::warn!("shutdown timeout reached with requests in flight; abandoning them");
    }
    if tokio::time::timeout_at(deadline, cache.flush_uploads())
        .await
        .is_err()
    {
        let dropped = cache.pending_uploads();
        metrics::counter!("mammoth_bucket_uploads_dropped_total", "reason" => "shutdown")
            .increment(dropped);
        tracing::warn!(
            dropped,
            "shutdown timeout reached with bucket uploads queued; they stay on this node only"
        );
    }
    tracing::info!("shut down");
    Ok(())
}

/// Starts serving one accepted connection, after a TLS handshake if `tls` is set. The handshake
/// runs in the connection's own task, so a slow client cannot hold up the accept loop.
fn accept(
    accepted: io::Result<(TcpStream, std::net::SocketAddr)>,
    app: &Router,
    graceful: &GracefulShutdown,
    settings: ServerSettings,
    tls: Option<&Tls>,
) {
    let stream = match accepted {
        Ok((stream, _)) => stream,
        Err(error) => {
            // Usually out of file descriptors; the next accept may succeed.
            tracing::warn!(%error, "could not accept a connection");
            return;
        }
    };
    // Small responses (HEAD answers, misses) must not wait for Nagle's algorithm.
    if let Err(error) = stream.set_nodelay(true) {
        tracing::debug!(%error, "could not set TCP_NODELAY");
    }
    // Taken now, so the drain also waits for connections still in their handshake.
    let watcher = graceful.watcher();
    let app = app.clone();
    let Some(tls) = tls else {
        tokio::spawn(serve_connection(stream, app, watcher, settings));
        return;
    };
    let acceptor = tls.acceptor.clone();
    let handshake_timeout = tls.handshake_timeout;
    tokio::spawn(async move {
        let reason = match tokio::time::timeout(handshake_timeout, acceptor.accept(stream)).await {
            Ok(Ok(stream)) => return serve_connection(stream, app, watcher, settings).await,
            // Scanners and plain-HTTP clients do this all day: not worth more than debug.
            Ok(Err(error)) => {
                tracing::debug!(%error, "TLS handshake failed");
                "error"
            }
            Err(_) => {
                tracing::debug!("TLS handshake timed out");
                "timeout"
            }
        };
        metrics::counter!("mammoth_tls_handshake_failures_total", "reason" => reason).increment(1);
    });
}

async fn serve_connection<S>(stream: S, app: Router, watcher: Watcher, settings: ServerSettings)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let connection = http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(settings.idle_timeout)
        .keep_alive(true)
        .serve_connection(TokioIo::new(stream), TowerToHyperService::new(app));
    if let Err(error) = watcher.watch(connection).await {
        tracing::debug!(%error, "connection ended with an error");
    }
}

/// Fails a request whose body sends nothing for the timeout. Per read, not in total, so a large
/// upload over a slow link is not cut off.
async fn body_read_timeout(
    State(timeout): State<Duration>,
    request: Request,
    next: Next,
) -> Response {
    let (parts, body) = request.into_parts();
    let chunks = body.into_data_stream();
    let timed = stream::unfold(Some(chunks), move |chunks| async move {
        let mut chunks = chunks?;
        match tokio::time::timeout(timeout, chunks.next()).await {
            Ok(Some(Ok(chunk))) => Some((Ok(chunk), Some(chunks))),
            Ok(Some(Err(error))) => Some((Err(io::Error::other(error)), None)),
            Ok(None) => None,
            Err(_) => Some((
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the request body sent nothing for too long",
                )),
                None,
            )),
        }
    });
    next.run(Request::from_parts(parts, Body::from_stream(timed)))
        .await
}
