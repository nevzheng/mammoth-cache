//! Bazel's HTTP remote cache: `GET`, `HEAD` and `PUT` on `/bazel/ac/{digest}` and
//! `/bazel/cas/{digest}` (design doc 0012). Bazel is pointed at it with
//! `--remote_cache=https://<host>/bazel`.
//!
//! Only the plain HTTP cache. Bazel's gRPC Remote Execution API (REAPI) is out of scope: no remote
//! execution and no gRPC cache.
//!
//! Digests are SHA-256, Bazel's default `--digest_function`. A CAS upload must hash to its digest,
//! checked as it streams; one that does not gets `400` and is not stored. Action cache entries are
//! stored as sent, and a store replaces the entry. An oversized store gets `413`: Bazel only warns
//! about a failed upload and keeps using the cache.

use std::{fmt, io};

use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use futures_util::{StreamExt, stream};
use sha2::{Digest, Sha256};

use super::{AppState, entries, error::ApiError};
use crate::{
    cache::{self, CacheError},
    key::{CacheKey, Namespace},
    store::ByteStream,
};

pub(super) async fn load_action(
    State(state): State<AppState>,
    Path(digest): Path<String>,
) -> Result<Response, ApiError> {
    entries::load(&state, &CacheKey::bazel(Namespace::BazelAc, &digest)?).await
}

pub(super) async fn check_action(
    State(state): State<AppState>,
    Path(digest): Path<String>,
) -> Result<Response, ApiError> {
    entries::check(&state, &CacheKey::bazel(Namespace::BazelAc, &digest)?).await
}

pub(super) async fn store_action(
    State(state): State<AppState>,
    Path(digest): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, ApiError> {
    let key = CacheKey::bazel(Namespace::BazelAc, &digest)?;
    entries::store(&state, &key, &headers, body).await
}

pub(super) async fn load_blob(
    State(state): State<AppState>,
    Path(digest): Path<String>,
) -> Result<Response, ApiError> {
    entries::load(&state, &CacheKey::bazel(Namespace::BazelCas, &digest)?).await
}

pub(super) async fn check_blob(
    State(state): State<AppState>,
    Path(digest): Path<String>,
) -> Result<Response, ApiError> {
    entries::check(&state, &CacheKey::bazel(Namespace::BazelCas, &digest)?).await
}

pub(super) async fn store_blob(
    State(state): State<AppState>,
    Path(digest): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, ApiError> {
    let key = CacheKey::bazel(Namespace::BazelCas, &digest)?;
    let expected = decode_hex(key.name());
    let declared_size = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    entries::store_through(&state, &key, &headers, body, |stream| {
        verify_sha256(stream, expected, declared_size)
    })
    .await
    .map_err(|e| match e {
        Failure::Mismatch => ApiError::digest_mismatch(&key),
        Failure::Api(e) => e,
    })
}

/// The 32 bytes of a digest that [`CacheKey::bazel`] has already validated as 64 lowercase hex
/// characters.
fn decode_hex(digest: &str) -> [u8; 32] {
    let nibble = |b: u8| match b {
        b'0'..=b'9' => b - b'0',
        _ => b - b'a' + 10,
    };
    let mut out = [0_u8; 32];
    for (byte, [high, low]) in out.iter_mut().zip(digest.as_bytes().as_chunks::<2>().0) {
        *byte = (nibble(*high) << 4) | nibble(*low);
    }
    out
}

/// Passes `body` through while hashing it, and ends it with a [`DigestMismatch`] error instead of
/// cleanly when the content does not hash to `expected`, so no tier keeps the entry.
///
/// With a declared length the check runs as the last byte arrives, before that chunk is passed on,
/// because a store that knows the length may never poll for the end of the stream.
fn verify_sha256(body: ByteStream, expected: [u8; 32], declared_size: Option<u64>) -> ByteStream {
    struct Verify {
        body: ByteStream,
        hasher: Option<Sha256>,
        seen: u64,
    }
    let check = move |hasher: Sha256| -> io::Result<()> {
        if hasher.finalize().as_slice() == expected {
            Ok(())
        } else {
            Err(io::Error::other(DigestMismatch))
        }
    };
    let state = Verify {
        body,
        hasher: Some(Sha256::new()),
        seen: 0,
    };
    stream::unfold(state, move |mut state| async move {
        let mut hasher = state.hasher.take()?;
        match state.body.next().await {
            Some(Ok(chunk)) => {
                hasher.update(&chunk);
                state.seen += chunk.len() as u64;
                if declared_size == Some(state.seen) {
                    // Done: a clean end after this chunk, or the mismatch in its place.
                    return Some((check(hasher).map(|()| chunk), state));
                }
                state.hasher = Some(hasher);
                Some((Ok(chunk), state))
            }
            Some(Err(e)) => Some((Err(e), state)),
            None => check(hasher).err().map(|e| (Err(e), state)),
        }
    })
    .boxed()
}

/// A CAS upload whose content does not hash to its digest.
#[derive(Debug)]
struct DigestMismatch;

impl fmt::Display for DigestMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("content does not hash to its digest")
    }
}

impl std::error::Error for DigestMismatch {}

/// Why a CAS store failed: a digest mismatch, found in the storage error it surfaced as, or any
/// other storage error.
enum Failure {
    Mismatch,
    Api(ApiError),
}

impl From<CacheError> for Failure {
    fn from(e: CacheError) -> Self {
        let mismatch = match &e {
            CacheError::Unavailable(source) | CacheError::Internal(source) => {
                cache::find::<DigestMismatch>(source).is_some()
            }
            CacheError::TooLarge { .. } | CacheError::Upload(_) => false,
        };
        if mismatch {
            Self::Mismatch
        } else {
            Self::Api(e.into())
        }
    }
}
