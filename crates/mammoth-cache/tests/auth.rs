//! Authentication and authorization over HTTP (design doc 0002, `api/openapi/common.yaml`).

// Helpers panic on unexpected values, like the tests that call them.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode, header},
    response::Response,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use http_body_util::BodyExt;
use mammoth_cache::{
    auth::{Authenticator, Scope, Scopes, hash_token},
    cache::Cache,
    router_with_auth,
    store::memory::MemoryStore,
};
use serde_json::Value;
use tower::ServiceExt;

const KEY: &str = "/cache/a3f5e1c09b2d4e7f";

fn app(anonymous: &[Scope], protect_metrics: bool) -> Router {
    let cache = Cache::builder()
        .bounded_tier(Arc::new(MemoryStore::new(1 << 20)))
        .build();
    let auth = Authenticator::new(
        anonymous.iter().copied().collect(),
        [
            (
                "ci".to_owned(),
                vec![hash_token("ci-token")],
                [Scope::CacheRead, Scope::CacheWrite]
                    .into_iter()
                    .collect::<Scopes>(),
            ),
            (
                "dev".to_owned(),
                vec![hash_token("dev-token")],
                [Scope::CacheRead].into_iter().collect(),
            ),
        ],
        protect_metrics,
    );
    router_with_auth(Arc::new(cache), None, Arc::new(auth))
}

fn basic(user: &str, token: &str) -> String {
    format!("Basic {}", STANDARD.encode(format!("{user}:{token}")))
}

async fn send(app: &Router, method: Method, uri: &str, auth: Option<&str>) -> Response {
    let mut request = Request::builder().method(method.clone()).uri(uri);
    if let Some(auth) = auth {
        request = request.header(header::AUTHORIZATION, auth);
    }
    let body = if method == Method::PUT {
        Body::from("entry")
    } else {
        Body::empty()
    };
    app.clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap()
}

async fn error(response: Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    json["error"]["details"][0].clone()
}

fn challenge(response: &Response) -> &str {
    response.headers()[header::WWW_AUTHENTICATE]
        .to_str()
        .unwrap()
}

#[tokio::test]
async fn a_user_with_the_scope_gets_through() {
    let app = app(&[], false);
    let ci = basic("ci", "ci-token");
    let put = send(&app, Method::PUT, KEY, Some(&ci)).await;
    assert_eq!(put.status(), StatusCode::CREATED);
    let get = send(&app, Method::GET, KEY, Some(&basic("dev", "dev-token"))).await;
    assert_eq!(get.status(), StatusCode::OK);
}

#[tokio::test]
async fn no_credentials_is_401_with_a_basic_challenge() {
    let app = app(&[], false);
    for (method, uri) in [
        (Method::GET, KEY),
        (Method::PUT, KEY),
        (Method::GET, "/maven/v1.1/g/a/cs/buildinfo.xml"),
        (Method::GET, "/v1/serverStatus"),
    ] {
        let response = send(&app, method, uri, None).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(challenge(&response), r#"Basic realm="mammoth-cache""#);
        assert_eq!(error(response).await["reason"], "CREDENTIALS_MISSING");
    }
}

#[tokio::test]
async fn wrong_credentials_are_401_invalid() {
    let app = app(&[Scope::CacheRead], false);
    for auth in [
        basic("ci", "wrong"),
        basic("nobody", "ci-token"),
        "Bearer wrong".to_owned(),
        "Basic !!!".to_owned(),
        "Digest x".to_owned(),
    ] {
        let response = send(&app, Method::GET, KEY, Some(&auth)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{auth}");
        assert_eq!(error(response).await["reason"], "CREDENTIALS_INVALID");
    }
}

#[tokio::test]
async fn a_user_without_the_scope_is_403_naming_it() {
    let app = app(&[], false);
    let response = send(&app, Method::PUT, KEY, Some(&basic("dev", "dev-token"))).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let info = error(response).await;
    assert_eq!(info["reason"], "SCOPE_MISSING");
    assert_eq!(info["metadata"]["user"], "dev");
    assert_eq!(info["metadata"]["requiredScope"], "cache:write");
}

#[tokio::test]
async fn bearer_tokens_work_too() {
    let app = app(&[], false);
    let response = send(&app, Method::PUT, KEY, Some("Bearer ci-token")).await;
    assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn anonymous_scopes_apply_without_credentials() {
    let app = app(&[Scope::CacheRead], false);
    assert_eq!(
        send(&app, Method::HEAD, KEY, None).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&app, Method::PUT, KEY, None).await.status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn health_checks_are_always_open_and_metrics_when_unprotected() {
    let open = app(&[], false);
    for uri in ["/ping", "/livez", "/readyz", "/metrics"] {
        let status = send(&open, Method::GET, uri, None).await.status();
        assert_ne!(status, StatusCode::UNAUTHORIZED, "{uri}");
    }
    let protected = app(&[], true);
    let response = send(&protected, Method::GET, "/metrics", None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
