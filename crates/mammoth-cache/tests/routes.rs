//! How routes answer requests they don't serve: methods a route lacks, a protocol that is off,
//! and Maven's `OPTIONS`, with authentication on.

// Helpers panic on unexpected values, like the tests that call them.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode, header},
    response::Response,
};
use http_body_util::BodyExt;
use mammoth_cache::{
    auth::{Authenticator, Scope, hash_token},
    cache::Cache,
    config::Protocols,
    router_with,
    store::memory::MemoryStore,
};
use serde_json::Value;
use tower::ServiceExt;

/// `ci:ci-token`, base64.
const CI: &str = "Basic Y2k6Y2ktdG9rZW4=";

/// A node with authentication on: anonymous requests get nothing, `ci` may read and write.
fn app(protocols: Protocols) -> Router {
    let cache = Cache::builder()
        .bounded_tier(Arc::new(MemoryStore::new(1 << 20)))
        .build();
    let auth = Authenticator::new(
        std::iter::empty().collect(),
        [(
            "ci".to_owned(),
            vec![hash_token("ci-token")],
            [Scope::CacheRead, Scope::CacheWrite].into_iter().collect(),
        )],
        false,
    );
    router_with(Arc::new(cache), None, Arc::new(auth), protocols)
}

async fn send(app: &Router, method: Method, uri: &str, auth: Option<&str>) -> Response {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(auth) = auth {
        request = request.header(header::AUTHORIZATION, auth);
    }
    app.clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn error_reason(response: Response) -> String {
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body).unwrap();
    json["error"]["details"][0]["reason"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn a_method_a_route_lacks_gets_an_aip_405() {
    let app = app(Protocols {
        bazel: true,
        ..Protocols::default()
    });
    let digest = "0".repeat(64);
    for uri in [
        "/cache/a3f5e1c09b2d4e7f".to_owned(),
        format!("/bazel/ac/{digest}"),
        format!("/bazel/cas/{digest}"),
    ] {
        for method in [Method::POST, Method::DELETE, Method::PATCH] {
            let response = send(&app, method.clone(), &uri, Some(CI)).await;
            assert_eq!(
                response.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} {uri}"
            );
            assert_eq!(response.headers()[header::ALLOW], "GET, HEAD, PUT");
            assert_eq!(error_reason(response).await, "METHOD_NOT_ALLOWED");
        }
    }
}

#[tokio::test]
async fn maven_options_needs_no_credentials() {
    let app = app(Protocols::default());
    let uri = "/maven/v1.2/com.example/app/0123/app.jar";
    let response = send(&app, Method::OPTIONS, uri, None).await;
    assert!(response.status().is_success(), "{}", response.status());
    // Everything else on the path still does.
    let response = send(&app, Method::GET, uri, None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_protocol_that_is_off_looks_like_an_unknown_route() {
    let app = app(Protocols {
        gradle: false,
        maven: false,
        bazel: false,
    });
    let digest = "0".repeat(64);
    for uri in [
        "/cache/a3f5e1c09b2d4e7f".to_owned(),
        "/maven/v1.2/com.example/app/0123/app.jar".to_owned(),
        format!("/bazel/ac/{digest}"),
        format!("/bazel/cas/{digest}"),
    ] {
        let response = send(&app, Method::GET, &uri, None).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(error_reason(response).await, "ROUTE_NOT_FOUND");
    }
}
