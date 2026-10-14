//! Arbitrary requests against the HTTP router: whatever the method, path or query, the answer is
//! never a `500`, and every error is an AIP-193 body in the `mammoth-cache.dev` domain
//! (design doc 0017). Paths mix the routes with bytes axum and the key parsers must refuse:
//! `%FF`, `%00`, `..`, `%2F` and repeated query keys.

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
use proptest::prelude::*;
use serde_json::Value;
use tower::ServiceExt;

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

fn method() -> impl Strategy<Value = Method> {
    prop_oneof![
        Just(Method::GET),
        Just(Method::HEAD),
        Just(Method::PUT),
        Just(Method::POST),
        Just(Method::DELETE),
        Just(Method::OPTIONS),
        Just(Method::PATCH),
        Just(Method::from_bytes(b"MKCOL").unwrap()),
    ]
}

fn segment() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("cache".to_owned()),
        Just("maven".to_owned()),
        Just("bazel".to_owned()),
        Just("ac".to_owned()),
        Just("cas".to_owned()),
        Just("v1".to_owned()),
        Just("serverStatus".to_owned()),
        Just("readyz".to_owned()),
        Just("metrics".to_owned()),
        Just("ping".to_owned()),
        Just("%FF".to_owned()),
        Just("%00".to_owned()),
        Just("..".to_owned()),
        Just("%2F".to_owned()),
        Just(String::new()),
        Just("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_owned()),
        "[a-f0-9]{1,70}",
        "[A-Za-z0-9._~-]{1,12}",
    ]
}

fn query() -> impl Strategy<Value = Option<String>> {
    prop_oneof![
        Just(None),
        Just(Some("verbose".to_owned())),
        Just(Some("verbose=1&verbose=2".to_owned())),
        Just(Some("%FF=%FF".to_owned())),
        "[a-z=&%0-9]{0,16}".prop_map(Some),
    ]
}

fn uri() -> impl Strategy<Value = String> {
    (prop::collection::vec(segment(), 1..5), query()).prop_map(|(segments, query)| {
        let path = format!("/{}", segments.join("/"));
        match query {
            Some(query) => format!("{path}?{query}"),
            None => path,
        }
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn every_error_is_aip_193_and_none_is_internal(method in method(), uri in uri()) {
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let (status, body) = runtime.block_on(async {
            let request = Request::builder()
                .method(method.clone())
                .uri(&uri)
                .body(Body::from("x"))
                .unwrap();
            let response = app().oneshot(request).await.unwrap();
            let status = response.status();
            let body = response.into_body().collect().await.unwrap().to_bytes();
            (status, body)
        });
        // `501` is designed: `/metrics` with metrics off. A `500` never is.
        prop_assert_ne!(status, StatusCode::INTERNAL_SERVER_ERROR, "{} {}", method, uri);
        if !status.is_success() && method != Method::HEAD {
            let json: Value = serde_json::from_slice(&body)
                .map_err(|e| TestCaseError::fail(format!("{method} {uri}: {status}, {e}")))?;
            let error = &json["error"];
            prop_assert_eq!(error["code"].as_u64(), Some(u64::from(status.as_u16())));
            prop_assert_eq!(error["details"][0]["domain"].as_str(), Some("mammoth-cache.dev"));
            prop_assert!(error["details"][0]["reason"].is_string());
        }
    }
}
