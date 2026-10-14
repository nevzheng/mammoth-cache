//! The HTTP/1.1 server: accepts connections, applies timeouts, and drains on shutdown (design doc
//! 0013).
//!
//! `axum::serve` sets no timeouts and waits for open connections without limit, so this module
//! serves each connection with hyper directly.

use std::{
    future::Future,
    io,
    pin::{Pin, pin},
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

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
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    time::{Instant, Sleep},
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
    /// How long a response may make no progress, because the client stopped reading, before the
    /// connection is closed.
    pub send_timeout: Duration,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            shutdown_delay: Duration::from_secs(5),
            shutdown_timeout: Duration::from_secs(25),
            idle_timeout: Duration::from_secs(75),
            body_read_timeout: Duration::from_secs(60),
            send_timeout: Duration::from_secs(60),
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
    let mut paused = AcceptErrors::default();
    let mut signal = pin!(signal);
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    paused.recovered();
                    accept(stream, &app, &graceful, settings, tls.as_ref());
                }
                Err(error) => paused.failed(&error).await,
            },
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
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    paused.recovered();
                    accept(stream, &app, &graceful, settings, tls.as_ref());
                }
                Err(error) => paused.failed(&error).await,
            },
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

/// How long the accept loop pauses after an error such as running out of file descriptors. Without
/// the pause the listener stays readable and the loop spins, logging at full speed.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Accept errors, logged once when they start and once when they stop.
#[derive(Default)]
struct AcceptErrors {
    failing: bool,
}

impl AcceptErrors {
    async fn failed(&mut self, error: &io::Error) {
        // The client gave up before the accept: nothing is wrong with the node.
        if matches!(
            error.kind(),
            io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset
        ) {
            tracing::debug!(%error, "connection closed before it was accepted");
            return;
        }
        if !self.failing {
            self.failing = true;
            // Usually out of file descriptors; connections wait in the backlog meanwhile.
            tracing::warn!(%error, "cannot accept connections; retrying every 100ms");
        }
        tokio::time::sleep(ACCEPT_BACKOFF).await;
    }

    fn recovered(&mut self) {
        if self.failing {
            self.failing = false;
            tracing::info!("accepting connections again");
        }
    }
}

/// Starts serving one accepted connection, after a TLS handshake if `tls` is set. The handshake
/// runs in the connection's own task, so a slow client cannot hold up the accept loop.
fn accept(
    stream: TcpStream,
    app: &Router,
    graceful: &GracefulShutdown,
    settings: ServerSettings,
    tls: Option<&Tls>,
) {
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
        .serve_connection(
            TokioIo::new(SendTimeout::new(stream, settings.send_timeout)),
            TowerToHyperService::new(app),
        );
    if let Err(error) = watcher.watch(connection).await {
        tracing::debug!(%error, "connection ended with an error");
    }
}

/// Fails a connection's writes once one makes no progress for the timeout: a client that stops
/// reading a large response would otherwise hold the connection, its task and the entry's file
/// open for good. Per write, not in total, so a slow download is not cut off.
struct SendTimeout<S> {
    inner: S,
    timeout: Duration,
    /// Running while a write is pending; reset by any progress.
    stalled: Option<Pin<Box<Sleep>>>,
}

impl<S> SendTimeout<S> {
    fn new(inner: S, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            stalled: None,
        }
    }

    /// Passes a write's result through, or fails it once it has been pending for the timeout.
    fn watch<T>(&mut self, cx: &mut Context<'_>, poll: Poll<io::Result<T>>) -> Poll<io::Result<T>> {
        if poll.is_ready() {
            self.stalled = None;
            return poll;
        }
        let timeout = self.timeout;
        let stalled = self
            .stalled
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(timeout)));
        match stalled.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the client stopped reading the response",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for SendTimeout<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for SendTimeout<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write(cx, buf);
        self.watch(cx, poll)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        self.watch(cx, poll)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let poll = Pin::new(&mut self.inner).poll_flush(cx);
        self.watch(cx, poll)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
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
