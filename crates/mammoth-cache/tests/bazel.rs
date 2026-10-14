//! Bazel's HTTP remote cache, `/bazel/ac/{digest}` and `/bazel/cas/{digest}` (design doc 0012),
//! and the `[protocols]` switches that mount it.

// Helpers panic on unexpected values, like the tests that call them.
#![allow(clippy::unwrap_used)]

use std::{fmt::Write, sync::Arc};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
    response::Response,
};
use bytes::Bytes;
use futures_util::stream;
use http_body_util::BodyExt;
use mammoth_cache::{
    auth::{Authenticator, Scope},
    cache::Cache,
    config::Protocols,
    router_with,
    store::{
        disk::{DiskConfig, DiskStore},
        memory::MemoryStore,
    },
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

const BAZEL_ONLY: Protocols = Protocols {
    gradle: false,
    maven: false,
    bazel: true,
    reapi: false,
};

fn app_over(cache: Cache, protocols: Protocols) -> Router {
    router_with(
        Arc::new(cache),
        None,
        Arc::new(Authenticator::open()),
        protocols,
    )
}

fn memory_cache(max_entry_size: u64) -> Cache {
    Cache::builder()
        .bounded_tier(Arc::new(MemoryStore::new(1 << 20)))
        .max_entry_size(max_entry_size)
        .build()
}

fn app(protocols: Protocols) -> Router {
    app_over(memory_cache(1 << 16), protocols)
}

fn sha256(body: &[u8]) -> String {
    Sha256::digest(body)
        .iter()
        .fold(String::new(), |mut hex, b| {
            write!(hex, "{b:02x}").unwrap();
            hex
        })
}

async fn send(app: &Router, request: Request<Body>) -> Response {
    app.clone().oneshot(request).await.unwrap()
}

async fn body_bytes(response: Response) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

async fn error_reason(response: Response) -> String {
    let json: Value = serde_json::from_slice(&body_bytes(response).await).unwrap();
    json["error"]["details"][0]["reason"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn put(uri: &str, body: &'static [u8]) -> Request<Body> {
    Request::put(uri)
        .header(header::CONTENT_LENGTH, body.len())
        .body(Body::from(body))
        .unwrap()
}

/// A `PUT` with no `Content-Length`, its body in two chunks.
fn put_chunked(uri: &str, first: &'static [u8], second: &'static [u8]) -> Request<Body> {
    let chunks = stream::iter([
        Ok::<_, std::io::Error>(Bytes::from_static(first)),
        Ok(Bytes::from_static(second)),
    ]);
    Request::put(uri).body(Body::from_stream(chunks)).unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

fn head(uri: &str) -> Request<Body> {
    Request::head(uri).body(Body::empty()).unwrap()
}

#[tokio::test]
async fn cas_store_then_load() {
    let app = app(BAZEL_ONLY);
    let blob: &[u8] = b"compiled output";
    let uri = format!("/bazel/cas/{}", sha256(blob));

    assert_eq!(send(&app, get(&uri)).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        send(&app, put(&uri, blob)).await.status(),
        StatusCode::CREATED
    );
    // CAS entries are immutable: a second store writes nothing.
    assert_eq!(send(&app, put(&uri, blob)).await.status(), StatusCode::OK);

    let response = send(&app, head(&uri)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "15");
    let response = send(&app, get(&uri)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await, blob);
}

#[tokio::test]
async fn cas_rejects_content_that_does_not_match_its_digest() {
    let app = app(BAZEL_ONLY);
    let uri = format!("/bazel/cas/{}", sha256(b"what the digest says"));

    let response = send(&app, put(&uri, b"something else")).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(error_reason(response).await, "DIGEST_MISMATCH");
    // Nothing was kept.
    assert_eq!(send(&app, get(&uri)).await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cas_checks_the_digest_without_a_content_length() {
    let app = app(BAZEL_ONLY);
    let good = format!("/bazel/cas/{}", sha256(b"hello world"));
    let bad = format!("/bazel/cas/{}", sha256(b"hello there"));

    let response = send(&app, put_chunked(&good, b"hello ", b"world")).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let response = send(&app, put_chunked(&bad, b"hello ", b"world")).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(error_reason(response).await, "DIGEST_MISMATCH");
    assert_eq!(send(&app, get(&bad)).await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cas_digest_mismatch_leaves_nothing_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let disk = DiskStore::open(DiskConfig {
        path: dir.path().to_owned(),
        capacity_bytes: 1 << 20,
        max_entry_age: None,
        free_space_buffer: 0,
    })
    .await
    .unwrap();
    let cache = Cache::builder()
        .bounded_tier(Arc::new(MemoryStore::new(1 << 20)))
        .bounded_tier(Arc::new(disk))
        .max_entry_size(1 << 16)
        .build();
    let app = app_over(cache, BAZEL_ONLY);
    let uri = format!("/bazel/cas/{}", sha256(b"expected"));

    let response = send(&app, put(&uri, b"tampered")).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(error_reason(response).await, "DIGEST_MISMATCH");
    assert_eq!(send(&app, get(&uri)).await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn ac_entries_are_stored_as_sent_and_replaced() {
    let app = app(BAZEL_ONLY);
    // An action digest names the action, not the result, so the body never matches it.
    let uri = format!("/bazel/ac/{}", sha256(b"some action"));

    assert_eq!(
        send(&app, put(&uri, b"result one")).await.status(),
        StatusCode::CREATED
    );
    assert_eq!(
        send(&app, put(&uri, b"result two")).await.status(),
        StatusCode::CREATED
    );
    let response = send(&app, get(&uri)).await;
    assert_eq!(body_bytes(response).await, b"result two");
}

#[tokio::test]
async fn ac_and_cas_do_not_share_entries() {
    let app = app(BAZEL_ONLY);
    let blob: &[u8] = b"shared bytes";
    let digest = sha256(blob);

    send(&app, put(&format!("/bazel/cas/{digest}"), blob)).await;
    let response = send(&app, get(&format!("/bazel/ac/{digest}"))).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn invalid_digest_is_400() {
    let app = app(BAZEL_ONLY);
    for uri in [
        "/bazel/cas/abc",
        "/bazel/ac/E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855",
        "/bazel/cas/%FF",
    ] {
        let response = send(&app, get(uri)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(error_reason(response).await, "INVALID_DIGEST");
    }
}

#[tokio::test]
async fn oversized_store_is_413() {
    let app = app_over(memory_cache(4), BAZEL_ONLY);
    let blob: &[u8] = b"too large";
    let response = send(&app, put(&format!("/bazel/cas/{}", sha256(blob)), blob)).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn bazel_is_off_by_default() {
    let app = app(Protocols::default());
    let uri = format!("/bazel/cas/{}", sha256(b"x"));
    let response = send(&app, get(&uri)).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(error_reason(response).await, "ROUTE_NOT_FOUND");
}

#[tokio::test]
async fn protocols_that_are_off_have_no_routes() {
    let app = app(BAZEL_ONLY);
    for uri in [
        "/cache/a3f5e1c09b2d4e7f",
        "/maven/v1.2/com.example/app/0123/app.jar",
    ] {
        let response = send(&app, get(uri)).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(error_reason(response).await, "ROUTE_NOT_FOUND", "{uri}");
    }
    assert_eq!(send(&app, get("/ping")).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn bazel_routes_need_cache_scopes() {
    let cache = memory_cache(1 << 16);
    let auth = Authenticator::new(
        [Scope::CacheRead].into_iter().collect(),
        std::iter::empty(),
        false,
    );
    let app = router_with(Arc::new(cache), None, Arc::new(auth), BAZEL_ONLY);
    let blob: &[u8] = b"blob";
    let uri = format!("/bazel/cas/{}", sha256(blob));

    assert_eq!(send(&app, get(&uri)).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        send(&app, put(&uri, blob)).await.status(),
        StatusCode::UNAUTHORIZED
    );
}
