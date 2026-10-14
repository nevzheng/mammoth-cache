//! The access log (design doc 0018): one `INFO` line on target `mammoth::access` for each HTTP
//! request and gRPC call, written when the response ends or the client goes away, so the sizes
//! and the duration cover the whole transfer. Health checks and `/metrics` are not logged.
//!
//! The adapters say what a request did through [`user`], [`outcome`] and [`digests`], which write
//! to the request being logged and do nothing outside one. A line names the key, the result and
//! the tier only when the request touched exactly one entry.
//!
//! `client.address` is the TCP peer, unless the peer is a trusted proxy: then `X-Forwarded-For`
//! is read from the right, past the trusted proxies, to the first address that is not one.

use std::{
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::{
        Arc, Mutex, OnceLock, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Instant,
};

use axum::{
    body::{Body, Bytes, HttpBody},
    extract::{ConnectInfo, MatchedPath, Request},
    http::{HeaderMap, header},
    middleware::Next,
    response::Response,
};
use hyper::body::{Frame, SizeHint};
use ipnet::IpNet;
use tracing::Level;

use crate::{cache::StoreOutcome, key::CacheKey, store::Tier};

/// The `tracing` target every access line is written on.
pub const TARGET: &str = "mammoth::access";

/// Paths not logged: probes and scrapes would drown the log, and metrics count them.
const QUIET_PATHS: &[&str] = &["/ping", "/livez", "/readyz", "/metrics"];

struct Settings {
    enabled: bool,
    trusted_proxies: Vec<IpNet>,
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();

/// Sets whether the access log is written and which peers are trusted to report the client's
/// address. Once per process, before serving; until then it is on and trusts no proxy.
pub fn configure(enabled: bool, trusted_proxies: Vec<IpNet>) {
    let _ = SETTINGS.set(Settings {
        enabled,
        trusted_proxies,
    });
}

fn enabled() -> bool {
    SETTINGS.get().is_none_or(|s| s.enabled) && tracing::enabled!(target: TARGET, Level::INFO)
}

fn trusted_proxies() -> &'static [IpNet] {
    SETTINGS.get().map_or(&[], |s| &s.trusted_proxies)
}

/// What the adapters report about the request being logged.
#[derive(Default)]
struct Note {
    user: Option<String>,
    key: Option<String>,
    result: Option<&'static str>,
    tier: Option<Tier>,
    /// How many entries the request touched; the key is logged only for one.
    outcomes: u32,
    digests: Option<usize>,
}

tokio::task_local! {
    static NOTE: Arc<Mutex<Note>>;
}

fn note(f: impl FnOnce(&mut Note)) {
    let _ = NOTE.try_with(|note| f(&mut note.lock().unwrap_or_else(PoisonError::into_inner)));
}

/// The authenticated user the request runs as. Never called for credentials that failed, whose
/// user name may be a pasted token.
pub(crate) fn user(name: &str) {
    note(|n| n.user = Some(name.to_owned()));
}

/// What the request did to `key`: `hit` (from `tier`, when known), `miss`, `stored`, `exists` or
/// `too_large`.
pub(crate) fn outcome(key: &CacheKey, result: &'static str, tier: Option<Tier>) {
    note(|n| {
        n.outcomes += 1;
        n.key = Some(key.to_string());
        n.result = Some(result);
        n.tier = tier;
    });
}

/// What a store of `key` did.
pub(crate) fn stored(key: &CacheKey, done: StoreOutcome) {
    let result = match done {
        StoreOutcome::Stored => "stored",
        StoreOutcome::Exists => "exists",
        StoreOutcome::Dropped => "too_large",
    };
    outcome(key, result, None);
}

/// How many digests a batch gRPC call carried.
pub(crate) fn digests(count: usize) {
    note(|n| n.digests = Some(count));
}

/// The client's address: the peer, or, when the peer is a trusted proxy, the right-most address in
/// `X-Forwarded-For` that is not a trusted proxy. When every address is trusted it is the left-most;
/// a malformed entry stops the walk at the last good one.
fn client_address(peer: IpAddr, headers: &HeaderMap, trusted: &[IpNet]) -> IpAddr {
    let is_trusted = |addr: &IpAddr| trusted.iter().any(|net| net.contains(addr));
    if !is_trusted(&peer) {
        return peer;
    }
    let hops: Vec<Option<IpAddr>> = headers
        .get_all("x-forwarded-for")
        .iter()
        .flat_map(|value| match value.to_str() {
            Ok(value) => value.split(',').map(parse_hop).collect(),
            Err(_) => vec![None],
        })
        .collect();
    let mut client = peer;
    for hop in hops.into_iter().rev() {
        let Some(addr) = hop else { break };
        client = addr;
        if !is_trusted(&addr) {
            break;
        }
    }
    client
}

/// One `X-Forwarded-For` entry: an address, with or without a port.
fn parse_hop(hop: &str) -> Option<IpAddr> {
    let hop = hop.trim();
    let addr = hop
        .parse::<IpAddr>()
        .or_else(|_| hop.parse::<SocketAddr>().map(|s| s.ip()))
        .ok()?;
    Some(addr.to_canonical())
}

/// The protocol an HTTP path belongs to.
fn protocol(path: &str) -> &'static str {
    if path.starts_with("/cache/") {
        "gradle"
    } else if path.starts_with("/maven/") {
        "maven"
    } else if path.starts_with("/bazel/") {
        "bazel"
    } else if path.starts_with("/v1/") {
        "admin"
    } else {
        "other"
    }
}

/// Middleware that writes an access line for each HTTP request.
pub(crate) async fn http(request: Request, next: Next) -> Response {
    let path = request.uri().path();
    if QUIET_PATHS.contains(&path) || !enabled() {
        return next.run(request).await;
    }
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_owned());
    let kind = Kind::Http {
        method: request.method().as_str().to_owned(),
        path: path.to_owned(),
        route,
        status: None,
    };
    let (line, request) = Line::start(kind, request);
    let note = line.note.clone();
    let response = NOTE.scope(note, next.run(request)).await;
    let mut line = line;
    if let Kind::Http { status, .. } = &mut line.kind {
        *status = Some(response.status().as_u16());
    }
    response.map(|body| Body::new(Logged { body, line }))
}

/// Middleware that writes an access line for each gRPC call.
pub(crate) async fn grpc(request: Request, next: Next) -> Response {
    if !enabled() {
        return next.run(request).await;
    }
    let (service, method) = request
        .uri()
        .path()
        .strip_prefix('/')
        .and_then(|p| p.split_once('/'))
        .map_or_else(
            || (String::new(), request.uri().path().to_owned()),
            |(s, m)| (s.to_owned(), m.to_owned()),
        );
    let kind = Kind::Grpc {
        service,
        method,
        code: None,
    };
    let (line, request) = Line::start(kind, request);
    let note = line.note.clone();
    let response = NOTE.scope(note, next.run(request)).await;
    let mut line = line;
    // A trailers-only response: the call failed before answering.
    if let Some(found) = grpc_code(response.headers())
        && let Kind::Grpc { code, .. } = &mut line.kind
    {
        *code = Some(found);
    }
    response.map(|body| Body::new(Logged { body, line }))
}

fn grpc_code(headers: &HeaderMap) -> Option<i32> {
    headers.get("grpc-status")?.to_str().ok()?.parse().ok()
}

enum Kind {
    Http {
        method: String,
        path: String,
        route: Option<String>,
        /// `None` until the handler answers.
        status: Option<u16>,
    },
    Grpc {
        service: String,
        method: String,
        code: Option<i32>,
    },
}

/// One request, written to the log when dropped.
struct Line {
    kind: Kind,
    client: Option<IpAddr>,
    peer: Option<IpAddr>,
    user_agent: Option<String>,
    note: Arc<Mutex<Note>>,
    bytes_in: Arc<AtomicU64>,
    bytes_out: u64,
    started: Instant,
    ended: bool,
}

impl Line {
    fn start(kind: Kind, request: Request) -> (Self, Request) {
        let started = Instant::now();
        let peer = request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip().to_canonical());
        let client = peer.map(|peer| client_address(peer, request.headers(), trusted_proxies()));
        let user_agent = request
            .headers()
            .get(header::USER_AGENT)
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
        let bytes_in = Arc::new(AtomicU64::new(0));
        let counted = bytes_in.clone();
        let request = request.map(|body| {
            Body::new(Counted {
                body,
                read: counted,
            })
        });
        let line = Self {
            kind,
            client,
            peer,
            user_agent,
            note: Arc::default(),
            bytes_in,
            bytes_out: 0,
            started,
            ended: false,
        };
        (line, request)
    }
}

impl Drop for Line {
    #[allow(clippy::cast_precision_loss)] // Microseconds; exact well past any request's length.
    fn drop(&mut self) {
        let note = std::mem::take(&mut *self.note.lock().unwrap_or_else(PoisonError::into_inner));
        let duration_ms = self.started.elapsed().as_micros() as f64 / 1000.0;
        let user = note.user.as_deref().unwrap_or("anonymous");
        let single = note.outcomes == 1;
        let key = note.key.as_deref().filter(|_| single);
        let tier = note.tier.filter(|_| single).map(Tier::as_str);
        let refused = |result: Option<&'static str>, refused: bool| {
            result.filter(|_| single).or(refused.then_some("refused"))
        };
        let client = self.client.map(|c| c.to_string());
        let peer = self
            .peer
            .filter(|peer| Some(*peer) != self.client)
            .map(|p| p.to_string());
        let bytes_in = self.bytes_in.load(Ordering::Relaxed);
        let aborted = (!self.ended).then_some(true);
        match &self.kind {
            Kind::Http {
                method,
                path,
                route,
                status,
            } => {
                let result = refused(note.result, matches!(status, Some(401 | 403)));
                tracing::info!(
                    target: TARGET,
                    // A plain field first: a quoted name there reads as the format string.
                    message = "request",
                    "http.request.method" = method.as_str(),
                    "url.path" = path.as_str(),
                    "http.route" = route.as_deref(),
                    "http.response.status_code" = *status,
                    "http.request.body.size" = bytes_in,
                    "http.response.body.size" = self.bytes_out,
                    "client.address" = client.as_deref(),
                    "network.peer.address" = peer.as_deref(),
                    "user_agent.original" = self.user_agent.as_deref(),
                    "user.name" = user,
                    "mammoth.protocol" = protocol(path),
                    "mammoth.cache.key" = key,
                    "mammoth.cache.result" = result,
                    "mammoth.cache.tier" = tier,
                    "mammoth.duration_ms" = duration_ms,
                    "mammoth.aborted" = aborted,
                );
            }
            Kind::Grpc {
                service,
                method,
                code,
            } => {
                // No status by the end: the client cancelled (1) or the connection broke (2).
                let code = code.unwrap_or(if self.ended { 2 } else { 1 });
                let result = refused(note.result, matches!(code, 7 | 16));
                tracing::info!(
                    target: TARGET,
                    // A plain field first: a quoted name there reads as the format string.
                    message = "call",
                    "rpc.system" = "grpc",
                    "rpc.service" = service.as_str(),
                    "rpc.method" = method.as_str(),
                    "rpc.grpc.status_code" = code,
                    "http.request.body.size" = bytes_in,
                    "http.response.body.size" = self.bytes_out,
                    "client.address" = client.as_deref(),
                    "network.peer.address" = peer.as_deref(),
                    "user_agent.original" = self.user_agent.as_deref(),
                    "user.name" = user,
                    "mammoth.protocol" = "reapi",
                    "mammoth.digests" = note.digests,
                    "mammoth.cache.key" = key,
                    "mammoth.cache.result" = result,
                    "mammoth.cache.tier" = tier,
                    "mammoth.duration_ms" = duration_ms,
                    "mammoth.aborted" = aborted,
                );
            }
        }
    }
}

/// A request body that counts the bytes read from it.
struct Counted {
    body: Body,
    read: Arc<AtomicU64>,
}

impl HttpBody for Counted {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let poll = Pin::new(&mut self.body).poll_frame(cx);
        if let Poll::Ready(Some(Ok(frame))) = &poll
            && let Some(data) = frame.data_ref()
        {
            self.read.fetch_add(data.len() as u64, Ordering::Relaxed);
        }
        poll
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

/// A response body that counts what it sends and writes its request's line when dropped.
struct Logged {
    body: Body,
    line: Line,
}

impl HttpBody for Logged {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let poll = Pin::new(&mut self.body).poll_frame(cx);
        match &poll {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    self.line.bytes_out += data.len() as u64;
                }
                if let Some(trailers) = frame.trailers_ref()
                    && let Some(found) = grpc_code(trailers)
                    && let Kind::Grpc { code, .. } = &mut self.line.kind
                {
                    *code = Some(found);
                }
            }
            Poll::Ready(None) => self.line.ended = true,
            // A response that broke off is as incomplete as one the client walked away from.
            Poll::Ready(Some(Err(_))) | Poll::Pending => {}
        }
        poll
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

impl Drop for Logged {
    fn drop(&mut self) {
        // hyper may never poll a body it knows is empty, as for `HEAD`; one that has sent all it
        // said it would has ended too.
        if self.body.is_end_stream() || self.body.size_hint().exact() == Some(self.line.bytes_out) {
            self.line.ended = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn nets(list: &[&str]) -> Vec<IpNet> {
        list.iter().map(|n| n.parse().unwrap()).collect()
    }

    fn forwarded(values: &[&'static str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append("x-forwarded-for", HeaderValue::from_static(value));
        }
        headers
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn an_untrusted_peer_is_the_client() {
        let headers = forwarded(&["1.2.3.4"]);
        assert_eq!(
            client_address(ip("10.0.0.7"), &headers, &[]),
            ip("10.0.0.7")
        );
        let trusted = nets(&["192.168.0.0/16"]);
        assert_eq!(
            client_address(ip("10.0.0.7"), &headers, &trusted),
            ip("10.0.0.7")
        );
    }

    #[test]
    fn walks_past_trusted_proxies_from_the_right() {
        let trusted = nets(&["10.0.0.0/8"]);
        // A client can prepend anything; only what the proxies appended counts.
        let headers = forwarded(&["6.6.6.6, 1.2.3.4, 10.0.0.9"]);
        assert_eq!(
            client_address(ip("10.0.0.7"), &headers, &trusted),
            ip("1.2.3.4")
        );
        // Several headers are one list, in order.
        let headers = forwarded(&["6.6.6.6", "1.2.3.4:5678", "10.0.0.9"]);
        assert_eq!(
            client_address(ip("10.0.0.7"), &headers, &trusted),
            ip("1.2.3.4")
        );
        let headers = forwarded(&["[2001:db8::1]:443"]);
        assert_eq!(
            client_address(ip("10.0.0.7"), &headers, &trusted),
            ip("2001:db8::1")
        );
    }

    #[test]
    fn stops_at_the_left_most_or_a_malformed_entry() {
        let trusted = nets(&["10.0.0.0/8"]);
        let headers = forwarded(&["10.0.0.3, 10.0.0.9"]);
        assert_eq!(
            client_address(ip("10.0.0.7"), &headers, &trusted),
            ip("10.0.0.3")
        );
        let headers = forwarded(&["1.2.3.4, garbage, 10.0.0.9"]);
        assert_eq!(
            client_address(ip("10.0.0.7"), &headers, &trusted),
            ip("10.0.0.9")
        );
        assert_eq!(
            client_address(ip("10.0.0.7"), &HeaderMap::new(), &trusted),
            ip("10.0.0.7")
        );
    }

    #[test]
    fn maps_ipv4_in_ipv6_to_ipv4() {
        let headers = forwarded(&["::ffff:1.2.3.4"]);
        let trusted = nets(&["10.0.0.0/8"]);
        assert_eq!(
            client_address(ip("10.0.0.7"), &headers, &trusted),
            ip("1.2.3.4")
        );
    }

    #[test]
    fn names_protocols() {
        assert_eq!(protocol("/cache/abc"), "gradle");
        assert_eq!(protocol("/maven/v1/a"), "maven");
        assert_eq!(protocol("/bazel/cas/x"), "bazel");
        assert_eq!(protocol("/v1/serverStatus"), "admin");
        assert_eq!(protocol("/nope"), "other");
    }
}
