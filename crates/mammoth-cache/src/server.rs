//! The servers: HTTP/1.1 for the REST protocols and HTTP/2 for gRPC. Both accept connections,
//! apply timeouts, and drain on shutdown the same way (design doc 0013).
//!
//! `axum::serve` sets no timeouts and waits for open connections without limit, so this module
//! serves each connection with hyper directly.

use std::{
    convert::Infallible,
    future::Future,
    io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    pin::{Pin, pin},
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use axum::{
    Router,
    body::Body,
    extract::{ConnectInfo, Request, State},
    http,
    middleware::{self, Next},
    response::Response,
    routing::future::RouteFuture,
};
use futures_util::{StreamExt, stream, stream::FuturesUnordered};
use hyper::{
    body::Incoming,
    server::conn::{http1, http2},
};
use hyper_util::{
    rt::{TokioExecutor, TokioIo, TokioTimer},
    server::graceful::{GracefulShutdown, Watcher},
    service::TowerToHyperService,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpSocket, TcpStream},
    sync::watch,
    task::JoinHandle,
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
    /// How long an HTTP connection may wait for, and take to send, its next request's headers.
    /// hyper starts its header timer when a keep-alive connection goes idle, so one timeout covers
    /// both. On the gRPC port, how long a new connection may send nothing.
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

/// Pending connections the kernel queues for the accept loop, as `TcpListener::bind` uses.
const BACKLOG: u32 = 1024;

/// Binds a listener on `addr`. On the unspecified IPv6 address (`[::]`, the default) it accepts
/// IPv4 too, whatever the host's `bindv6only` default; on a host without IPv6 it falls back to
/// `0.0.0.0` on the same port. Any other address is bound as given.
pub fn bind(addr: SocketAddr) -> io::Result<TcpListener> {
    match listen_on(addr) {
        Err(error) if addr.ip() == Ipv6Addr::UNSPECIFIED && no_ipv6(&error) => {
            let fallback = SocketAddr::from((Ipv4Addr::UNSPECIFIED, addr.port()));
            tracing::info!(%addr, %fallback, %error, "no IPv6 on this host; listening on IPv4 only");
            listen_on(fallback)
        }
        result => result,
    }
}

fn listen_on(addr: SocketAddr) -> io::Result<TcpListener> {
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    if addr.ip() == Ipv6Addr::UNSPECIFIED {
        // IPv4 clients arrive as IPv4-mapped addresses.
        socket2::SockRef::from(&socket).set_only_v6(false)?;
    }
    #[cfg(unix)]
    socket.set_reuseaddr(true)?;
    socket.bind(addr)?;
    socket.listen(BACKLOG)
}

/// Whether binding failed because the host has no IPv6: the kernel lacks it, or it is turned off.
fn no_ipv6(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::AddrNotAvailable {
        return true;
    }
    #[cfg(unix)]
    if rustix::io::Errno::from_io_error(error) == Some(rustix::io::Errno::AFNOSUPPORT) {
        return true;
    }
    false
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
///
/// Steps 2 and 3 share the `shutdown_timeout` deadline; whatever is left then is abandoned. Queued
/// bucket uploads are not flushed here but by [`run_node`], once every listener has drained.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    cache: Arc<Cache>,
    settings: ServerSettings,
    signal: impl Future<Output = ()>,
) -> io::Result<()> {
    run(
        listener,
        None,
        app,
        Protocol::Http1,
        Some(&cache),
        settings,
        signal,
    )
    .await
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
    run(
        listener,
        Some(tls),
        app,
        Protocol::Http1,
        Some(&cache),
        settings,
        signal,
    )
    .await
}

/// Serves gRPC `app` over HTTP/2 on `listener`, over TLS when `tls` is set, until `signal`
/// resolves; then drains like [`serve`]: accept for `shutdown_delay`, send `GOAWAY`, and let calls
/// in flight finish until `shutdown_timeout` after the signal. Readiness is the HTTP port's.
///
/// A connection must send something within `idle_timeout` of opening, and one that has not sent
/// its first request when the drain starts is closed at once, so silent connections hold neither
/// file descriptors nor the drain. Request bodies and responses get the same per-read and
/// per-write timeouts as HTTP.
pub(crate) async fn serve_http2(
    listener: TcpListener,
    tls: Option<TlsSettings>,
    app: Router,
    settings: ServerSettings,
    signal: impl Future<Output = ()>,
) -> io::Result<()> {
    let tls = tls.map(|tls| Tls {
        acceptor: TlsAcceptor::from(tls.config),
        handshake_timeout: tls.handshake_timeout,
    });
    run(listener, tls, app, Protocol::Http2, None, settings, signal).await
}

/// A spawned server, named for logs, as [`run_node`] takes it.
pub type Server = (&'static str, JoinHandle<io::Result<()>>);

/// Runs a node's servers to the end. Each drains once `stop` is set, by the shutdown signal; when
/// every one has, the bucket uploads they queued finish within what is left of `shutdown_timeout`
/// from the signal. Flushing earlier would drop what a server still draining stores.
///
/// If a server stops before `stop` is set, which only a bug can cause, the others are stopped too
/// and this returns an error, so the orchestrator restarts the process rather than leaving
/// `/readyz` passing with a port dead.
///
/// # Errors
///
/// The first server's error, or an error if one stopped before `stop` was set.
pub async fn run_node(
    cache: &Cache,
    servers: Vec<Server>,
    stop: &watch::Sender<bool>,
    shutdown_timeout: Duration,
) -> io::Result<()> {
    let signalled = {
        let mut stopped = stop.subscribe();
        tokio::spawn(async move {
            let _ = stopped.wait_for(|stop| *stop).await;
            Instant::now()
        })
    };
    let mut running: FuturesUnordered<_> = servers
        .into_iter()
        .map(|(name, server)| async move { (name, server.await) })
        .collect();
    let mut result = Ok(());
    while let Some((name, joined)) = running.next().await {
        let mut served = joined.unwrap_or_else(|error| Err(io::Error::other(error)));
        if !*stop.borrow() {
            match &served {
                Ok(()) => tracing::error!(server = name, "server stopped; shutting down"),
                Err(error) => {
                    tracing::error!(server = name, %error, "server failed; shutting down");
                }
            }
            stop.send_replace(true);
            served = served.and(Err(io::Error::other(format!("the {name} server stopped"))));
        }
        result = result.and(served);
    }
    if let Ok(signalled) = signalled.await {
        flush_uploads(cache, signalled + shutdown_timeout).await;
    }
    tracing::info!("shut down");
    result
}

/// Finishes queued bucket uploads by `deadline`, counting and logging those left behind.
async fn flush_uploads(cache: &Cache, deadline: Instant) {
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
}

/// What a listener speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protocol {
    /// HTTP/1.1, for the REST protocols.
    Http1,
    /// HTTP/2 only, for gRPC.
    Http2,
}

impl Protocol {
    fn name(self) -> &'static str {
        match self {
            Self::Http1 => "HTTP",
            Self::Http2 => "gRPC",
        }
    }
}

/// HTTP/2 pings that find dead peers on idle gRPC connections.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// Calls one gRPC connection may have open at once. Each can buffer a message of up to
/// [`crate::reapi::MAX_MESSAGE_BYTES`] while it decodes.
const MAX_CONCURRENT_STREAMS: u32 = 128;

struct Tls {
    acceptor: TlsAcceptor,
    handshake_timeout: Duration,
}

async fn run(
    listener: TcpListener,
    tls: Option<Tls>,
    app: Router,
    protocol: Protocol,
    cache: Option<&Cache>,
    settings: ServerSettings,
    signal: impl Future<Output = ()>,
) -> io::Result<()> {
    let app = app.layer(middleware::from_fn_with_state(
        settings.body_read_timeout,
        body_read_timeout,
    ));
    let graceful = GracefulShutdown::new();
    // Set when the listener closes; HTTP/2 connections that have sent nothing yet close then.
    let (draining, _) = watch::channel(false);
    let mut paused = AcceptErrors::default();
    let mut signal = pin!(signal);
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    paused.recovered();
                    accept(stream, &app, protocol, &graceful, &draining, settings, tls.as_ref());
                }
                Err(error) => paused.failed(&error).await,
            },
            () = &mut signal => break,
        }
    }

    let deadline = Instant::now() + settings.shutdown_timeout;
    let server = protocol.name();
    if let Some(cache) = cache {
        cache.begin_shutdown();
    }
    tracing::info!(
        server,
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
                    accept(stream, &app, protocol, &graceful, &draining, settings, tls.as_ref());
                }
                Err(error) => paused.failed(&error).await,
            },
            () = &mut stop_accepting => break,
        }
    }
    drop(listener);
    draining.send_replace(true);

    tracing::info!(
        server,
        "no longer accepting connections; finishing requests in flight"
    );
    if tokio::time::timeout_at(deadline, graceful.shutdown())
        .await
        .is_err()
    {
        tracing::warn!(
            server,
            "shutdown timeout reached with requests in flight; abandoning them"
        );
    }
    tracing::info!(server, "drained");
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

/// The router for one connection, adding the peer's address to each request as [`ConnectInfo`].
#[derive(Clone)]
struct Connection {
    app: Router,
    peer: Option<SocketAddr>,
}

impl tower::Service<http::Request<Incoming>> for Connection {
    type Response = Response;
    type Error = Infallible;
    type Future = RouteFuture<Infallible>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        // A router is always ready.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut request: http::Request<Incoming>) -> Self::Future {
        if let Some(peer) = self.peer {
            request.extensions_mut().insert(ConnectInfo(peer));
        }
        self.app.call(request)
    }
}

/// Starts serving one accepted connection, after a TLS handshake if `tls` is set. The handshake
/// runs in the connection's own task, so a slow client cannot hold up the accept loop.
fn accept(
    stream: TcpStream,
    app: &Router,
    protocol: Protocol,
    graceful: &GracefulShutdown,
    draining: &watch::Sender<bool>,
    settings: ServerSettings,
    tls: Option<&Tls>,
) {
    // Small responses (HEAD answers, misses) must not wait for Nagle's algorithm.
    if let Err(error) = stream.set_nodelay(true) {
        tracing::debug!(%error, "could not set TCP_NODELAY");
    }
    // Taken now, so the drain also waits for connections still in their handshake.
    let watcher = graceful.watcher();
    let draining = draining.subscribe();
    // The peer's address, for the access log; a socket that already lost it is served anyway.
    let peer = stream.peer_addr().ok();
    let app = Connection {
        app: app.clone(),
        peer,
    };
    let Some(tls) = tls else {
        tokio::spawn(serve_connection(
            stream, app, protocol, watcher, draining, settings,
        ));
        return;
    };
    let acceptor = tls.acceptor.clone();
    let handshake_timeout = tls.handshake_timeout;
    tokio::spawn(async move {
        let reason = match tokio::time::timeout(handshake_timeout, acceptor.accept(stream)).await {
            Ok(Ok(stream)) => {
                return serve_connection(stream, app, protocol, watcher, draining, settings).await;
            }
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

async fn serve_connection<S>(
    stream: S,
    app: Connection,
    protocol: Protocol,
    watcher: Watcher,
    mut draining: watch::Receiver<bool>,
    settings: ServerSettings,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let io = SendTimeout::new(stream, settings.send_timeout);
    let served = match protocol {
        Protocol::Http1 => {
            let connection = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(settings.idle_timeout)
                .keep_alive(true)
                .serve_connection(TokioIo::new(io), TowerToHyperService::new(app));
            watcher.watch(connection).await
        }
        Protocol::Http2 => {
            let connection = http2::Builder::new(TokioExecutor::new())
                .timer(TokioTimer::new())
                .keep_alive_interval(Some(KEEPALIVE_INTERVAL))
                .keep_alive_timeout(KEEPALIVE_TIMEOUT)
                .max_concurrent_streams(MAX_CONCURRENT_STREAMS)
                .serve_connection(
                    TokioIo::new(FirstRead::new(io, async move {
                        // hyper waits for the client's preface even when draining, so give up on a
                        // connection that has sent nothing once the drain starts too.
                        let drain = draining.wait_for(|draining| *draining);
                        let _ = tokio::time::timeout(settings.idle_timeout, drain).await;
                    })),
                    TowerToHyperService::new(app),
                );
            watcher.watch(connection).await
        }
    };
    if let Err(error) = served {
        tracing::debug!(%error, "connection ended with an error");
    }
}

/// Fails a connection that sends nothing before `deadline` resolves. HTTP/2 has no header
/// timeout, so a client that connects and stays silent, such as a port scanner or an L4 health
/// check, would otherwise hold a file descriptor, and the drain, until it goes away.
struct FirstRead<S> {
    inner: S,
    /// Polled until the first byte arrives.
    deadline: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

impl<S> FirstRead<S> {
    fn new(inner: S, deadline: impl Future<Output = ()> + Send + 'static) -> Self {
        Self {
            inner,
            deadline: Some(Box::pin(deadline)),
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for FirstRead<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        let Some(deadline) = self.deadline.as_mut() else {
            return poll;
        };
        match poll {
            Poll::Ready(Ok(())) if buf.filled().len() > before => self.deadline = None,
            Poll::Ready(_) => {}
            Poll::Pending => {
                if deadline.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the connection sent nothing for too long",
                    )));
                }
            }
        }
        poll
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for FirstRead<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
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
