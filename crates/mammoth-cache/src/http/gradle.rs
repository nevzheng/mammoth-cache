//! The Gradle HTTP build cache protocol: `GET`, `HEAD` and `PUT` on `/cache/{key}`.
//!
//! Status codes follow Gradle's semantics over AIP-193 where they differ (design doc 0001).

use std::io;

use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::{StreamExt, TryStreamExt};

use super::{AppState, error::ApiError};
use crate::{key::CacheKey, store::PutOutcome};

const OCTET_STREAM: HeaderValue = HeaderValue::from_static("application/octet-stream");

pub(super) async fn load_entry(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Response, ApiError> {
    let key = CacheKey::parse(&key)?;
    let hit = state
        .cache
        .get(&key)
        .await?
        .ok_or_else(|| ApiError::entry_not_found(key.as_str()))?;
    tracing::debug!(%key, tier = %hit.tier, size = hit.blob.size, "hit");
    let headers = [
        (header::CONTENT_TYPE, OCTET_STREAM),
        (header::CONTENT_LENGTH, HeaderValue::from(hit.blob.size)),
    ];
    Ok((headers, Body::from_stream(hit.blob.body)).into_response())
}

pub(super) async fn check_entry(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Response, ApiError> {
    let key = CacheKey::parse(&key)?;
    let size = state
        .cache
        .head(&key)
        .await?
        .ok_or_else(|| ApiError::entry_not_found(key.as_str()))?;
    let headers = [
        (header::CONTENT_TYPE, OCTET_STREAM),
        (header::CONTENT_LENGTH, HeaderValue::from(size)),
    ];
    Ok(headers.into_response())
}

/// Stores an entry. Any `Content-Type` is accepted: answering `415` would fail the build.
pub(super) async fn store_entry(
    State(state): State<AppState>,
    Path(key): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, ApiError> {
    let key = CacheKey::parse(&key)?;
    let declared_size = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    // Returning before the body is polled means hyper never sends `100 Continue`, so a rejected
    // upload is never transmitted.
    let stream = body.into_data_stream().map_err(io::Error::other).boxed();
    match state.cache.put(&key, declared_size, stream).await? {
        PutOutcome::Stored => Ok(StatusCode::CREATED),
        PutOutcome::Exists => Ok(StatusCode::OK),
    }
}
