//! Authentication and authorization for every route (design doc 0002).
//!
//! Each route needs one scope. The middleware reads the `Authorization` header (Basic with the
//! token as the password, or Bearer), finds the principal and either forwards the request, with
//! the [`Principal`] as an extension, or answers `401` or `403`. It runs before the handler reads
//! the body, so a refused upload with `Expect: 100-continue` is never sent. A refused upload sent
//! without it is read and discarded, so the client gets its answer and can reuse the connection:
//! up to the entry size limit for a known user, and up to [`ANONYMOUS_DRAIN_BYTES`] within
//! [`ANONYMOUS_DRAIN_TIME`] for anyone else, who could otherwise hold the connection.

use std::{sync::Arc, time::Duration};

use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::StreamExt;

use super::error::ApiError;
use crate::api::error::Reason;
use crate::{
    auth::{Authenticator, Credentials, Scope},
    config::Protocols,
};

/// What the middleware checks requests against.
#[derive(Clone)]
pub(crate) struct Guard {
    pub(crate) auth: Arc<Authenticator>,
    pub(crate) protocols: Protocols,
    /// The most of a refused upload's body that is read and discarded: the entry size limit,
    /// which any upload the node accepts may send anyway.
    pub(crate) drain_limit: u64,
}

/// The scope a request needs, or `None` for open routes: health checks, Maven's `OPTIONS`, and
/// unknown paths, which include those of a protocol that is off, so it answers like any other
/// unknown path.
fn required_scope(method: &Method, path: &str, guard: &Guard) -> Option<Scope> {
    let protocols = guard.protocols;
    let entries = (protocols.gradle && path.starts_with("/cache/"))
        || (protocols.maven && path.starts_with("/maven/"))
        || (protocols.bazel && path.starts_with("/bazel/"));
    if entries {
        if *method == Method::OPTIONS && path.starts_with("/maven/") {
            // Lists the methods; the contract asks no credentials for it.
            return None;
        }
        let reads = matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
        Some(if reads {
            Scope::CacheRead
        } else {
            Scope::CacheWrite
        })
    } else if path.starts_with("/v1/") {
        Some(Scope::AdminRead)
    } else if path == "/metrics" && guard.auth.protects_metrics() {
        Some(Scope::MetricsRead)
    } else {
        None
    }
}

/// Parses the `Authorization` header. `Err` for a header that is present but unusable.
fn credentials(headers: &HeaderMap) -> Result<Option<(String, String)>, ()> {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| ())?;
    let (scheme, rest) = value.split_once(' ').ok_or(())?;
    if scheme.eq_ignore_ascii_case("basic") {
        let decoded = STANDARD.decode(rest.trim()).map_err(|_| ())?;
        let decoded = String::from_utf8(decoded).map_err(|_| ())?;
        let (user, token) = decoded.split_once(':').ok_or(())?;
        Ok(Some((user.to_owned(), token.to_owned())))
    } else if scheme.eq_ignore_ascii_case("bearer") {
        // An empty user marks a Bearer token.
        Ok(Some((String::new(), rest.trim().to_owned())))
    } else {
        Err(())
    }
}

/// Checks the request's credentials against the scope its route needs.
pub(crate) async fn require(
    State(guard): State<Guard>,
    mut request: Request,
    next: Next,
) -> Response {
    let auth = &guard.auth;
    let Some(scope) = required_scope(request.method(), request.uri().path(), &guard) else {
        return next.run(request).await;
    };
    let Some((refusal, known)) = refuse(auth, scope, &mut request) else {
        return next.run(request).await;
    };
    let expects_continue = request
        .headers()
        .get(header::EXPECT)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"100-continue"));
    if expects_continue {
        // The client waits for `100 Continue`, which an unread body never sends.
        return refusal;
    }
    let body = request.into_body();
    let drained = if known {
        discard(body, guard.drain_limit).await
    } else {
        let limit = guard.drain_limit.min(ANONYMOUS_DRAIN_BYTES);
        tokio::time::timeout(ANONYMOUS_DRAIN_TIME, discard(body, limit))
            .await
            .unwrap_or(false)
    };
    if drained { refusal } else { close(refusal) }
}

/// The most of a refused body read from a client that is not a known user: enough for a small
/// upload to finish cleanly.
const ANONYMOUS_DRAIN_BYTES: u64 = 1024 * 1024;

/// How long reading it may take in all, so a client sending slowly cannot hold the connection.
const ANONYMOUS_DRAIN_TIME: Duration = Duration::from_secs(3);

/// Authenticates the request and checks it has `scope`: the refusal, and whether it was for a
/// known user lacking the scope; or `None` with the [`Principal`] added to the request.
fn refuse(auth: &Authenticator, scope: Scope, request: &mut Request) -> Option<(Response, bool)> {
    let Ok(presented) = credentials(request.headers()) else {
        return Some((invalid(), false));
    };
    let creds = presented.as_ref().map(|(user, token)| {
        if user.is_empty() {
            Credentials::Bearer { token }
        } else {
            Credentials::Basic { user, token }
        }
    });
    let Ok(principal) = auth.authenticate(creds) else {
        return Some((invalid(), false));
    };
    if principal.scopes.contains(scope) {
        request.extensions_mut().insert(principal);
        return None;
    }
    Some(match &principal.user {
        None => {
            count_failure(Reason::CredentialsMissing);
            let refusal = ApiError::unauthenticated(
                Reason::CredentialsMissing,
                "Authentication is required.",
            )
            .into_response();
            (refusal, false)
        }
        Some(user) => {
            count_failure(Reason::ScopeMissing);
            (
                ApiError::permission_denied(user, scope).into_response(),
                true,
            )
        }
    })
}

/// Reads and discards a refused request's body before answering, so the client finishes sending
/// and reads the answer instead of a reset. Whether it ended within `limit` bytes; if not, the rest
/// is left unread and the connection must close after the answer.
async fn discard(body: Body, limit: u64) -> bool {
    let mut stream = body.into_data_stream();
    let mut read = 0_u64;
    while let Some(Ok(chunk)) = stream.next().await {
        read += chunk.len() as u64;
        if read > limit {
            return false;
        }
    }
    true
}

/// The refusal, closing the connection after it: the rest of the body is never read.
fn close(mut refusal: Response) -> Response {
    refusal
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    refusal
}

fn invalid() -> Response {
    count_failure(Reason::CredentialsInvalid);
    ApiError::unauthenticated(Reason::CredentialsInvalid, "The credentials are not valid.")
        .into_response()
}

/// Counts a refused request by its AIP-193 `reason`, one of a fixed set.
fn count_failure(reason: Reason) {
    metrics::counter!("mammoth_auth_failures_total", "reason" => reason.as_str()).increment(1);
}
