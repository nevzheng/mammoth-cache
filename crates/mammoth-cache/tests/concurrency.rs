//! Many requests at once against a memory and disk cache: no lost, mixed or partial entries.

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
    cache::Cache,
    router,
    store::{
        disk::{DiskConfig, DiskStore},
        memory::MemoryStore,
    },
};
use tower::ServiceExt;

async fn app(dir: &tempfile::TempDir) -> Router {
    let disk = DiskStore::open(DiskConfig {
        path: dir.path().to_path_buf(),
        capacity_bytes: 64 << 20,
        max_entry_age: None,
    })
    .await
    .unwrap();
    let cache = Cache::builder()
        .bounded_tier(Arc::new(MemoryStore::new(16 << 20)))
        .bounded_tier(Arc::new(disk))
        .build();
    router(Arc::new(cache), None)
}

async fn call(app: &Router, method: Method, uri: &str, body: Vec<u8>) -> (StatusCode, Vec<u8>) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::from(body))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec())
}

/// An entry whose bytes identify it, so a mixed-up read is caught.
fn entry(id: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| u8::try_from((id * 31 + i) % 251).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_keys_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir).await;
    let tasks: Vec<_> = (0..64)
        .map(|id| {
            let app = app.clone();
            tokio::spawn(async move {
                let uri = format!("/cache/{id:04x}");
                let body = entry(id, 1000 + id * 997);
                let (put, _) = call(&app, Method::PUT, &uri, body.clone()).await;
                assert_eq!(put, StatusCode::CREATED);
                for _ in 0..4 {
                    let (status, got) = call(&app, Method::GET, &uri, Vec::new()).await;
                    assert_eq!(status, StatusCode::OK);
                    assert!(got == body, "entry {id} came back wrong");
                }
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_gradle_key_stored_by_many_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir).await;
    let body = entry(7, 300_000);
    let tasks: Vec<_> = (0..32)
        .map(|_| {
            let (app, body) = (app.clone(), body.clone());
            tokio::spawn(async move { call(&app, Method::PUT, "/cache/abcd", body).await.0 })
        })
        .collect();
    for task in tasks {
        assert!(task.await.unwrap().is_success());
    }
    let (status, got) = call(&app, Method::GET, "/cache/abcd", Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_maven_path_replaced_by_many_at_once_is_never_mixed() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir).await;
    let versions: Vec<Vec<u8>> = (0..16).map(|id| entry(id, 200_000)).collect();
    let tasks: Vec<_> = versions
        .iter()
        .cloned()
        .map(|body| {
            let app = app.clone();
            tokio::spawn(async move {
                call(&app, Method::PUT, "/maven/v1/g/a/buildinfo.xml", body)
                    .await
                    .0
            })
        })
        .collect();
    for task in tasks {
        assert!(task.await.unwrap().is_success());
    }
    let (status, got) = call(&app, Method::GET, "/maven/v1/g/a/buildinfo.xml", Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        versions.contains(&got),
        "the stored object is not any one upload"
    );
}
