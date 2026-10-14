//! The router against the `OpenAPI` contracts in `api/openapi/`: every path and method a contract
//! lists is routed, and every other method on those paths is refused with `405`. Catches drift in
//! `cargo test`; the end-to-end suite checks status codes and bodies against the contracts.

// Helpers panic on unexpected values, like the tests that call them.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode},
};
use http_body_util::BodyExt;
use mammoth_cache::{
    auth::Authenticator, cache::Cache, config::Protocols, router_with, store::memory::MemoryStore,
};
use serde_json::Value;
use tower::ServiceExt;

/// Every contract the server implements, by file name.
const CONTRACTS: [(&str, &str); 4] = [
    (
        "gradle-build-cache.v1.yaml",
        include_str!("../../../api/openapi/gradle-build-cache.v1.yaml"),
    ),
    (
        "maven-build-cache.v1.yaml",
        include_str!("../../../api/openapi/maven-build-cache.v1.yaml"),
    ),
    (
        "bazel-http-cache.v1.yaml",
        include_str!("../../../api/openapi/bazel-http-cache.v1.yaml"),
    ),
    (
        "ops.v1.yaml",
        include_str!("../../../api/openapi/ops.v1.yaml"),
    ),
];

/// The methods checked on every path.
const METHODS: [Method; 6] = [
    Method::GET,
    Method::HEAD,
    Method::PUT,
    Method::POST,
    Method::DELETE,
    Method::OPTIONS,
];

/// A path's operations, read from a contract's `paths:` block: path keys indented two spaces,
/// operation keys four. The contracts are hand-written in that layout, which Spectral and Redocly
/// check, so no YAML parser is needed.
fn operations(contract: &str) -> Vec<(String, Vec<Method>)> {
    let mut paths: Vec<(String, Vec<Method>)> = Vec::new();
    let mut in_paths = false;
    for line in contract.lines() {
        if !line.starts_with(' ') && !line.is_empty() {
            in_paths = line == "paths:";
            continue;
        }
        if !in_paths {
            continue;
        }
        if let Some(path) = line.strip_prefix("  /").and_then(|l| l.strip_suffix(':')) {
            paths.push((format!("/{path}"), Vec::new()));
        } else if let Some(key) = line.strip_prefix("    ").and_then(|l| l.strip_suffix(':'))
            && let Some(method) = METHODS
                .iter()
                .find(|m| m.as_str().eq_ignore_ascii_case(key))
        {
            paths.last_mut().unwrap().1.push(method.clone());
        }
    }
    paths
}

/// A concrete URI for a templated path, with a valid value for each parameter.
fn concrete(path: &str) -> String {
    path.replace("{key}", "a3f5e1c09b2d4e7f")
        .replace(
            "{path}",
            "v1.2/com.example/app/0123456789abcdef/buildinfo.xml",
        )
        .replace("{digest}", &"0".repeat(64))
}

fn app() -> Router {
    let cache = Cache::builder()
        .bounded_tier(Arc::new(MemoryStore::new(1 << 20)))
        .build();
    let every_protocol = Protocols {
        gradle: true,
        maven: true,
        bazel: true,
        reapi: false,
    };
    router_with(
        Arc::new(cache),
        None,
        Arc::new(Authenticator::open()),
        every_protocol,
    )
}

/// The status and, for a JSON error, its `ErrorInfo.reason`.
async fn call(app: &Router, method: &Method, uri: &str) -> (StatusCode, Option<String>) {
    let request = Request::builder()
        .method(method.clone())
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let reason = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|json| {
            json["error"]["details"][0]["reason"]
                .as_str()
                .map(str::to_owned)
        });
    (status, reason)
}

#[test]
fn every_contract_lists_paths() {
    for (name, contract) in CONTRACTS {
        let paths = operations(contract);
        assert!(!paths.is_empty(), "{name}: no paths found");
        for (path, methods) in paths {
            assert!(!methods.is_empty(), "{name}: {path} has no operations");
        }
    }
}

#[tokio::test]
async fn router_serves_exactly_the_contracted_methods() {
    let app = app();
    for (name, contract) in CONTRACTS {
        for (path, methods) in operations(contract) {
            let uri = concrete(&path);
            for method in &METHODS {
                let (status, reason) = call(&app, method, &uri).await;
                // axum answers `HEAD` on any `GET` route, which HTTP allows.
                let contracted = methods.contains(method)
                    || (*method == Method::HEAD && methods.contains(&Method::GET));
                if contracted {
                    assert!(
                        status != StatusCode::METHOD_NOT_ALLOWED
                            && reason.as_deref() != Some("ROUTE_NOT_FOUND"),
                        "{name}: {method} {path} is in the contract but not routed ({status})"
                    );
                } else {
                    assert_eq!(
                        status,
                        StatusCode::METHOD_NOT_ALLOWED,
                        "{name}: {method} {path} is routed but not in the contract"
                    );
                }
            }
        }
    }
}
