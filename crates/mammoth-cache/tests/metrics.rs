//! `/metrics` with a Prometheus recorder installed. Its own test binary, because the recorder is
//! process-wide.

// Helpers panic on unexpected values, like the tests that call them.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use mammoth_cache::{cache::Cache, router, store::memory::MemoryStore};
use metrics_exporter_prometheus::PrometheusBuilder;
use tower::ServiceExt;

#[tokio::test]
async fn counts_requests_by_client_and_gradle_version() {
    let handle = PrometheusBuilder::new().install_recorder().unwrap();
    let cache = Cache::builder()
        .bounded_tier(Arc::new(MemoryStore::new(1024)))
        .build();
    let app = router(Arc::new(cache), Some(handle));

    let lookup = Request::get("/cache/abc")
        .header(header::USER_AGENT, "Gradle/8.10 (Linux;6.1;amd64)")
        .header("x-gradle-version", "8.10.2")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(lookup).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let scrape = Request::get("/metrics").body(Body::empty()).unwrap();
    let response = app.oneshot(scrape).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = response.into_body().collect().await.unwrap().to_bytes();
    let text = std::str::from_utf8(&text).unwrap();
    assert!(
        text.lines()
            .any(|line| line.starts_with("mammoth_http_requests_total")
                && line.contains(r#"route="/cache/{key}""#)
                && line.contains(r#"client="gradle""#)
                && line.contains(r#"code="404""#)),
        "{text}"
    );
    assert!(
        text.contains(r#"mammoth_gradle_requests_total{gradle_version="8.10"} 1"#),
        "{text}"
    );
}
