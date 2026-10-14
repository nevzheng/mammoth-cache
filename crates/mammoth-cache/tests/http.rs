//! End-to-end tests of the HTTP surface against the contracts in `api/openapi/`.

// Helpers panic on unexpected values, like the tests that call them.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
    response::Response,
};
use http_body_util::BodyExt;
use mammoth_cache::{
    cache::{Cache, OversizePolicy},
    router,
    store::memory::MemoryStore,
};
use serde_json::Value;
use tower::ServiceExt;

const KEY: &str = "a3f5e1c09b2d4e7f8a1b6c3d5e9f0a2b";

fn app_with(max_entry_size: u64, oversize: OversizePolicy) -> Router {
    let cache = Cache::builder()
        .bounded_tier(Arc::new(MemoryStore::new(1 << 20)))
        .max_entry_size(max_entry_size)
        .oversize(oversize)
        .build();
    router(Arc::new(cache))
}

fn app(max_entry_size: u64) -> Router {
    app_with(max_entry_size, OversizePolicy::Drop)
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
    assert_eq!(json["error"]["details"][0]["domain"], "mammoth-cache.dev");
    json["error"]["details"][0]["reason"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn put(key: &str, body: &'static [u8]) -> Request<Body> {
    Request::put(format!("/cache/{key}"))
        .header(
            header::CONTENT_TYPE,
            "application/vnd.gradle.build-cache-artifact.v2",
        )
        .header(header::CONTENT_LENGTH, body.len())
        .body(Body::from(body))
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

#[tokio::test]
async fn ping_returns_success() {
    let response = send(&app(1024), get("/ping")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await, b"SUCCESS");
}

#[tokio::test]
async fn store_then_load() {
    let app = app(1024);

    let response = send(&app, put(KEY, b"entry bytes")).await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let response = send(&app, get(&format!("/cache/{KEY}"))).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/octet-stream"
    );
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "11");
    assert_eq!(body_bytes(response).await, b"entry bytes");

    let head = Request::head(format!("/cache/{KEY}"))
        .body(Body::empty())
        .unwrap();
    let response = send(&app, head).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "11");
}

#[tokio::test]
async fn storing_an_existing_key_answers_200() {
    let app = app(1024);
    send(&app, put(KEY, b"first")).await;
    let response = send(&app, put(KEY, b"second")).await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn miss_is_404_entry_not_found() {
    let response = send(&app(1024), get(&format!("/cache/{KEY}"))).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(error_reason(response).await, "ENTRY_NOT_FOUND");
}

#[tokio::test]
async fn invalid_key_is_400() {
    let response = send(&app(1024), get("/cache/NOT-HEX")).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(error_reason(response).await, "INVALID_CACHE_KEY");
}

#[tokio::test]
async fn oversized_entry_is_dropped_with_200() {
    let app = app(4);
    let response = send(&app, put(KEY, b"too large")).await;
    assert_eq!(response.status(), StatusCode::OK);

    let response = send(&app, get(&format!("/cache/{KEY}"))).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn oversized_entry_is_413_when_rejecting() {
    let app = app_with(4, OversizePolicy::Reject);
    let response = send(&app, put(KEY, b"too large")).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(error_reason(response).await, "ENTRY_TOO_LARGE");
}

#[tokio::test]
async fn server_status_uses_proto_json() {
    let app = app(1024);
    send(&app, put(KEY, b"x")).await;
    send(&app, get(&format!("/cache/{KEY}"))).await;

    let response = send(&app, get("/v1/serverStatus")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let json: Value = serde_json::from_slice(&body_bytes(response).await).unwrap();
    assert_eq!(json["name"], "serverStatus");
    assert_eq!(json["hitCount"], "1");
    assert_eq!(json["tiers"][0]["tier"], "TIER_MEMORY");
    assert_eq!(json["tiers"][0]["entryCount"], "1");
}

#[tokio::test]
async fn unknown_route_is_aip_193_not_found() {
    let response = send(&app(1024), get("/nope")).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(error_reason(response).await, "ROUTE_NOT_FOUND");
}
