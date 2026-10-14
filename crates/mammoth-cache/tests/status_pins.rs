//! Pins the HTTP status of every error the server can answer in-process, as main answers it
//! today. Design doc 0017 rebuilds error handling in steps; each step may change error bodies but
//! must pass this file unchanged, since Gradle, Maven and Bazel read only the status code.
//!
//! Some pinned statuses come with a body that 0017 calls a gap (plain-text rejections, empty
//! `405` bodies). The pin is the status; the bodies are fixed later without touching it.

// Helpers panic on unexpected values, like the tests that call them.
#![allow(clippy::unwrap_used)]

use std::{fmt::Write, io, sync::Arc};

use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::stream;
use mammoth_cache::{
    api::error::Reason,
    auth::{Authenticator, Scope, Scopes, hash_token},
    cache::{Cache, OversizePolicy},
    config::Protocols,
    key::CacheKey,
    router_with,
    store::{
        Blob, BlobStore, BoundedStore, BoxFuture, ByteStream, HealthCheck, HealthProblem,
        PutOutcome, Tier, TierUsage, memory::MemoryStore,
    },
};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

const ALL: Protocols = Protocols {
    gradle: true,
    maven: true,
    bazel: true,
    reapi: false,
};

const LIMIT: u64 = 16;
const KEY: &str = "a3f5e1c09b2d4e7f8a1b6c3d5e9f0a2b";
const MAVEN: &str = "/maven/v1.2/org.example/app/0123abcd/buildinfo.xml";

fn over_limit() -> &'static [u8] {
    &[b'x'; 32]
}

fn sha256(body: &[u8]) -> String {
    Sha256::digest(body).iter().fold(String::new(), |mut s, b| {
        write!(s, "{b:02x}").unwrap();
        s
    })
}

fn app_over(cache: Cache, auth: Authenticator) -> (Router, Arc<Cache>) {
    let cache = Arc::new(cache);
    let router = router_with(cache.clone(), None, Arc::new(auth), ALL);
    (router, cache)
}

fn memory(oversize: OversizePolicy) -> Cache {
    Cache::builder()
        .bounded_tier(Arc::new(MemoryStore::new(1 << 20)))
        .max_entry_size(LIMIT)
        .oversize(oversize)
        .build()
}

fn open(oversize: OversizePolicy) -> Router {
    app_over(memory(oversize), Authenticator::open()).0
}

/// A local tier whose stores fail with an `io::Error` of the given kind, and whose readiness
/// check reports a problem when asked to.
#[derive(Debug)]
struct Failing {
    kind: io::ErrorKind,
    unhealthy: bool,
}

impl BlobStore for Failing {
    fn tier(&self) -> Tier {
        Tier::Memory
    }

    fn get<'a>(&'a self, _key: &'a CacheKey) -> BoxFuture<'a, io::Result<Option<Blob>>> {
        Box::pin(async { Ok(None) })
    }

    fn head<'a>(&'a self, _key: &'a CacheKey) -> BoxFuture<'a, io::Result<Option<u64>>> {
        Box::pin(async { Ok(None) })
    }

    fn put<'a>(
        &'a self,
        _key: &'a CacheKey,
        _size_hint: Option<u64>,
        _body: ByteStream,
    ) -> BoxFuture<'a, io::Result<PutOutcome>> {
        let kind = self.kind;
        Box::pin(async move { Err(io::Error::new(kind, "the test tier fails")) })
    }
}

impl BoundedStore for Failing {
    fn usage(&self) -> TierUsage {
        TierUsage::default()
    }

    fn health(&self) -> Vec<HealthCheck> {
        vec![HealthCheck {
            name: "storage-writable",
            problem: self.unhealthy.then(|| HealthProblem {
                reason: Reason::StorageNotWritable,
                message: "The test tier is not writable.".to_owned(),
                metadata: Vec::new(),
            }),
        }]
    }
}

fn failing(kind: io::ErrorKind, unhealthy: bool) -> Router {
    let cache = Cache::builder()
        .bounded_tier(Arc::new(Failing { kind, unhealthy }))
        .max_entry_size(LIMIT)
        .build();
    app_over(cache, Authenticator::open()).0
}

fn authed() -> Router {
    let auth = Authenticator::new(
        Scopes::default(),
        [(
            "dev".to_owned(),
            vec![hash_token("dev-token")],
            [Scope::CacheRead].into_iter().collect::<Scopes>(),
        )],
        false,
    );
    app_over(memory(OversizePolicy::Drop), auth).0
}

fn basic(user: &str, token: &str) -> String {
    format!("Basic {}", STANDARD.encode(format!("{user}:{token}")))
}

fn request(method: Method, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

fn put(uri: &str, body: &'static [u8]) -> Request<Body> {
    Request::put(uri)
        .header(header::CONTENT_LENGTH, body.len())
        .body(Body::from(body))
        .unwrap()
}

/// A `PUT` whose body fails after its first chunk, the way a reset connection does.
fn broken_put(uri: &str) -> Request<Body> {
    let chunks: Vec<io::Result<&'static [u8]>> =
        vec![Ok(b"0123"), Err(io::Error::other("connection reset"))];
    Request::put(uri)
        .header(header::CONTENT_LENGTH, 8)
        .body(Body::from_stream(stream::iter(chunks)))
        .unwrap()
}

fn with_auth(mut request: Request<Body>, value: &str) -> Request<Body> {
    request
        .headers_mut()
        .insert(header::AUTHORIZATION, value.parse().unwrap());
    request
}

async fn status(app: &Router, request: Request<Body>) -> StatusCode {
    let response = app.clone().oneshot(request).await.unwrap();
    // Read the body so a status that depends on it (a failed upload) is final.
    let status = response.status();
    let _ = axum::body::to_bytes(response.into_body(), usize::MAX).await;
    status
}

/// Asserts each `(request, status)` pair, reporting every mismatch at once.
async fn pin(app: &Router, cases: Vec<(Request<Body>, StatusCode)>) {
    let mut wrong = Vec::new();
    for (request, want) in cases {
        let what = format!("{} {}", request.method(), request.uri());
        let got = status(app, request).await;
        if got != want {
            wrong.push(format!("{what}: got {got}, pinned {want}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "status pins broken:\n{}",
        wrong.join("\n")
    );
}

#[tokio::test]
async fn gradle_statuses() {
    let key = format!("/cache/{KEY}");
    pin(
        &open(OversizePolicy::Drop),
        vec![
            (request(Method::GET, &key), StatusCode::NOT_FOUND),
            (request(Method::HEAD, &key), StatusCode::NOT_FOUND),
            (
                request(Method::GET, "/cache/not-hex"),
                StatusCode::BAD_REQUEST,
            ),
            (request(Method::GET, "/cache/%FF"), StatusCode::BAD_REQUEST),
            (request(Method::PUT, "/cache/%FF"), StatusCode::BAD_REQUEST),
            (request(Method::HEAD, "/cache/%FF"), StatusCode::BAD_REQUEST),
            (request(Method::POST, &key), StatusCode::METHOD_NOT_ALLOWED),
            (
                request(Method::DELETE, &key),
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            (put(&key, over_limit()), StatusCode::OK),
            (broken_put(&key), StatusCode::BAD_REQUEST),
        ],
    )
    .await;
    pin(
        &open(OversizePolicy::Reject),
        vec![(put(&key, over_limit()), StatusCode::PAYLOAD_TOO_LARGE)],
    )
    .await;
}

#[tokio::test]
async fn maven_statuses() {
    pin(
        &open(OversizePolicy::Drop),
        vec![
            (request(Method::GET, MAVEN), StatusCode::NOT_FOUND),
            (request(Method::HEAD, MAVEN), StatusCode::NOT_FOUND),
            (
                request(Method::GET, "/maven/a/../b.xml"),
                StatusCode::BAD_REQUEST,
            ),
            (
                request(Method::GET, "/maven/a/%FF/b.xml"),
                StatusCode::BAD_REQUEST,
            ),
            (
                request(Method::DELETE, MAVEN),
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            (request(Method::POST, MAVEN), StatusCode::METHOD_NOT_ALLOWED),
            (put(MAVEN, over_limit()), StatusCode::PAYLOAD_TOO_LARGE),
            (broken_put(MAVEN), StatusCode::BAD_REQUEST),
        ],
    )
    .await;
}

#[tokio::test]
async fn bazel_statuses() {
    let missing = format!("/bazel/cas/{}", sha256(b"missing"));
    let mismatch = format!("/bazel/cas/{}", sha256(b"something else"));
    let ac = format!("/bazel/ac/{}", sha256(b"action"));
    pin(
        &open(OversizePolicy::Drop),
        vec![
            (request(Method::GET, &missing), StatusCode::NOT_FOUND),
            (request(Method::GET, &ac), StatusCode::NOT_FOUND),
            (
                request(Method::GET, "/bazel/cas/not-a-digest"),
                StatusCode::BAD_REQUEST,
            ),
            (
                request(Method::GET, "/bazel/ac/%FF"),
                StatusCode::BAD_REQUEST,
            ),
            (
                request(Method::GET, "/bazel/cas/%FF"),
                StatusCode::BAD_REQUEST,
            ),
            (
                request(Method::POST, &missing),
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            (put(&mismatch, b"0123"), StatusCode::BAD_REQUEST),
            (put(&ac, over_limit()), StatusCode::PAYLOAD_TOO_LARGE),
            (broken_put(&ac), StatusCode::BAD_REQUEST),
        ],
    )
    .await;
}

#[tokio::test]
async fn ops_statuses() {
    let app = open(OversizePolicy::Drop);
    pin(
        &app,
        vec![
            (
                request(Method::GET, "/no-such-route"),
                StatusCode::NOT_FOUND,
            ),
            (
                request(Method::GET, "/metrics"),
                StatusCode::NOT_IMPLEMENTED,
            ),
            (
                request(Method::GET, "/readyz?verbose=1&verbose=2"),
                StatusCode::BAD_REQUEST,
            ),
            (
                request(Method::POST, "/ping"),
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            (
                request(Method::PUT, "/livez"),
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            (
                request(Method::POST, "/readyz"),
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            (
                request(Method::POST, "/metrics"),
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            (
                request(Method::POST, "/v1/serverStatus"),
                StatusCode::METHOD_NOT_ALLOWED,
            ),
        ],
    )
    .await;

    let (app, cache) = app_over(memory(OversizePolicy::Drop), Authenticator::open());
    cache.begin_shutdown();
    pin(
        &app,
        vec![(
            request(Method::GET, "/readyz"),
            StatusCode::SERVICE_UNAVAILABLE,
        )],
    )
    .await;
}

#[tokio::test]
async fn auth_statuses() {
    let key = format!("/cache/{KEY}");
    pin(
        &authed(),
        vec![
            (request(Method::GET, &key), StatusCode::UNAUTHORIZED),
            (
                with_auth(request(Method::GET, &key), &basic("dev", "wrong")),
                StatusCode::UNAUTHORIZED,
            ),
            (
                with_auth(request(Method::GET, &key), "Bearer wrong"),
                StatusCode::UNAUTHORIZED,
            ),
            (
                with_auth(put(&key, b"0123"), &basic("dev", "dev-token")),
                StatusCode::FORBIDDEN,
            ),
        ],
    )
    .await;
}

#[tokio::test]
async fn storage_failure_statuses() {
    let key = format!("/cache/{KEY}");
    for (kind, want) in [
        (io::ErrorKind::TimedOut, StatusCode::SERVICE_UNAVAILABLE),
        (io::ErrorKind::WouldBlock, StatusCode::SERVICE_UNAVAILABLE),
        (
            io::ErrorKind::PermissionDenied,
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (io::ErrorKind::Other, StatusCode::INTERNAL_SERVER_ERROR),
    ] {
        let app = failing(kind, false);
        pin(
            &app,
            vec![(put(&key, b"0123"), want), (put(MAVEN, b"0123"), want)],
        )
        .await;
    }
    pin(
        &failing(io::ErrorKind::Other, true),
        vec![(
            request(Method::GET, "/readyz"),
            StatusCode::SERVICE_UNAVAILABLE,
        )],
    )
    .await;
}
