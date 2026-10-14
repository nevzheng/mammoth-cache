//! Load, check and store, shared by every protocol adapter. Each adapter parses its own key and
//! keeps its own status rules; the core decides immutability and oversize handling by namespace.

use std::io;

use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::{StreamExt, TryStreamExt};

use super::{AppState, error::ApiError};
use crate::{
    cache::{Admission, CacheError, StoreOutcome},
    key::CacheKey,
    store::ByteStream,
};

const OCTET_STREAM: HeaderValue = HeaderValue::from_static("application/octet-stream");

/// `GET`: the entry as `application/octet-stream`, or `404`.
pub(super) async fn load(state: &AppState, key: &CacheKey) -> Result<Response, ApiError> {
    let hit = state
        .cache
        .get(key)
        .await?
        .ok_or_else(|| ApiError::entry_not_found(key))?;
    tracing::debug!(%key, tier = %hit.tier, size = hit.blob.size, "hit");
    let headers = [
        (header::CONTENT_TYPE, OCTET_STREAM),
        (header::CONTENT_LENGTH, HeaderValue::from(hit.blob.size)),
    ];
    Ok((headers, Body::from_stream(hit.blob.body)).into_response())
}

/// `HEAD`: the entry's size, or `404`.
pub(super) async fn check(state: &AppState, key: &CacheKey) -> Result<Response, ApiError> {
    let size = state
        .cache
        .head(key)
        .await?
        .ok_or_else(|| ApiError::entry_not_found(key))?;
    let headers = [
        (header::CONTENT_TYPE, OCTET_STREAM),
        (header::CONTENT_LENGTH, HeaderValue::from(size)),
    ];
    Ok(headers.into_response())
}

/// `PUT`: `201` when written, `200` when nothing was written (already present, or dropped).
///
/// Any `Content-Type` is accepted. The size check happens before the body is read: hyper sends
/// `100 Continue` only when the body is first polled, so a client using `Expect: 100-continue`
/// never uploads a refused or dropped entry.
pub(super) async fn store(
    state: &AppState,
    key: &CacheKey,
    headers: &HeaderMap,
    body: Body,
) -> Result<StatusCode, ApiError> {
    store_through(state, key, headers, body, |stream| stream).await
}

/// [`store`], with the body passed through `wrap` on its way to the tiers, for an adapter that
/// checks the content as it streams. An error from the wrapped stream fails the store, and no tier
/// keeps the entry. Storage errors come back as `E`, so the adapter can recognize its own error
/// inside one.
pub(super) async fn store_through<E: From<CacheError>>(
    state: &AppState,
    key: &CacheKey,
    headers: &HeaderMap,
    body: Body,
    wrap: impl FnOnce(ByteStream) -> ByteStream,
) -> Result<StatusCode, E> {
    let declared_size = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    match state.cache.admit(key, declared_size) {
        Admission::Accept => {}
        Admission::Reject(e) => return Err(e.into()),
        Admission::Drop => {
            // Without `Expect: 100-continue` the body is already on its way. Read and discard it,
            // so the client finishes its upload and gets a clean response, not a reset.
            if !expects_continue(headers) {
                let mut stream = body.into_data_stream();
                while let Some(Ok(_)) = stream.next().await {}
            }
            return Ok(StatusCode::OK);
        }
    }
    let stream = wrap(body.into_data_stream().map_err(io::Error::other).boxed());
    match state.cache.put(key, declared_size, stream).await? {
        StoreOutcome::Stored => Ok(StatusCode::CREATED),
        StoreOutcome::Exists | StoreOutcome::Dropped => Ok(StatusCode::OK),
    }
}

fn expects_continue(headers: &HeaderMap) -> bool {
    headers
        .get(header::EXPECT)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"100-continue"))
}
