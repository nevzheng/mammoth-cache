//! The Gradle HTTP build cache protocol: `GET`, `HEAD` and `PUT` on `/cache/{key}`
//! (`api/openapi/gradle-build-cache.v1.yaml`).
//!
//! Gradle owns these status rules, and they override AIP-193 where they differ (design doc 0001):
//! any non-2xx store response turns the remote cache off for the rest of the build, so an
//! oversized entry is accepted and dropped with `200`, and any `Content-Type` is accepted (Gradle
//! sends `application/vnd.gradle.build-cache-artifact.v1` up to 8.7 and `.v2` from 8.8).

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Response,
};

use super::{AppState, entries, error::ApiError, name::Name};
use crate::key::CacheKey;

pub(super) async fn load_entry(
    State(state): State<AppState>,
    Name(key): Name,
) -> Result<Response, ApiError> {
    entries::load(&state, &CacheKey::gradle(&key)?).await
}

pub(super) async fn check_entry(
    State(state): State<AppState>,
    Name(key): Name,
) -> Result<Response, ApiError> {
    entries::check(&state, &CacheKey::gradle(&key)?).await
}

pub(super) async fn store_entry(
    State(state): State<AppState>,
    Name(key): Name,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, ApiError> {
    entries::store(&state, &CacheKey::gradle(&key)?, &headers, body).await
}
