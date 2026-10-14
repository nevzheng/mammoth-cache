//! Load, check and store, shared by every protocol adapter. Each adapter parses its own key and
//! keeps its own status rules; the core decides immutability and oversize handling by namespace.

use std::{
    io,
    pin::Pin,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, ready},
};

use axum::{
    body::{Body, BodyDataStream},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures_util::{Stream, StreamExt, TryStreamExt};

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
    let body = UploadBody::new(body);
    // Without `Expect: 100-continue` the body is on its way whatever the answer. Whenever the
    // store returns before reading all of it, read and discard the rest, so the client finishes
    // its upload and gets a clean response, not a reset. With `Expect` and an unread body, the
    // client is waiting for `100 Continue` and sends nothing.
    let in_flight = !expects_continue(headers);
    let result = match state.cache.admit(key, declared_size) {
        Admission::Accept => {
            let stream = wrap(body.clone().map_err(io::Error::other).boxed());
            state
                .cache
                .put(key, declared_size, stream)
                .await
                .map_err(E::from)
                .map(|outcome| match outcome {
                    StoreOutcome::Stored => StatusCode::CREATED,
                    StoreOutcome::Exists | StoreOutcome::Dropped => StatusCode::OK,
                })
        }
        Admission::Reject(e) => Err(e.into()),
        Admission::Drop => Ok(StatusCode::OK),
    };
    if in_flight || body.started() {
        body.drain().await;
    }
    result
}

/// A request body that can be handed to the store and still drained afterwards.
#[derive(Clone)]
struct UploadBody {
    /// `None` once the body has ended or failed, so it is never polled again.
    stream: Arc<Mutex<Option<BodyDataStream>>>,
    started: Arc<AtomicBool>,
}

impl UploadBody {
    fn new(body: Body) -> Self {
        Self {
            stream: Arc::new(Mutex::new(Some(body.into_data_stream()))),
            started: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Whether anything has read from the body, which sends `100 Continue` if it was expected.
    fn started(&self) -> bool {
        self.started.load(Ordering::Relaxed)
    }

    /// Reads and discards what is left of the body, stopping at the first error.
    async fn drain(mut self) {
        while let Some(Ok(_)) = self.next().await {}
    }
}

impl Stream for UploadBody {
    type Item = Result<Bytes, axum::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.started.store(true, Ordering::Relaxed);
        let mut stream = self.stream.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(body) = stream.as_mut() else {
            return Poll::Ready(None);
        };
        let item = ready!(body.poll_next_unpin(cx));
        if !matches!(item, Some(Ok(_))) {
            *stream = None;
        }
        Poll::Ready(item)
    }
}

fn expects_continue(headers: &HeaderMap) -> bool {
    headers
        .get(header::EXPECT)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"100-continue"))
}
