//! Authentication and authorization for every route (design doc 0002).
//!
//! Each route needs one scope. The middleware reads the `Authorization` header (Basic with the
//! token as the password, or Bearer), finds the principal and either forwards the request, with
//! the [`Principal`] as an extension, or answers `401` or `403`. It runs before the handler reads
//! the body, so a refused upload with `Expect: 100-continue` is never sent. A refused upload sent
//! without it is read and discarded, up to the entry size limit, so the client gets its answer and
//! can reuse the connection.

use std::sync::Arc;

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
    let Some(refusal) = refuse(auth, scope, &mut request) else {
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
    discard(request.into_body(), guard.drain_limit, refusal).await
}

/// Authenticates the request and checks it has `scope`: the refusal, or `None` with the
/// [`Principal`] added to the request.
fn refuse(auth: &Authenticator, scope: Scope, request: &mut Request) -> Option<Response> {
    let Ok(presented) = credentials(request.headers()) else {
        return Some(invalid());
    };
    let creds = presented.as_ref().map(|(user, token)| {
        if user.is_empty() {
            Credentials::Bearer { token }
        } else {
            Credentials::Basic { user, token }
        }
    });
    let Ok(principal) = auth.authenticate(creds) else {
        return Some(invalid());
    };
    if principal.scopes.contains(scope) {
        request.extensions_mut().insert(principal);
        return None;
    }
    Some(match &principal.user {
        None => ApiError::unauthenticated("CREDENTIALS_MISSING", "Authentication is required.")
            .into_response(),
        Some(user) => ApiError::permission_denied(user, scope).into_response(),
    })
}

/// Reads and discards a refused request's body before answering, so the client finishes sending
/// and reads the answer instead of a reset. A body over `limit` is left unread and the connection
/// closed after the answer.
async fn discard(body: Body, limit: u64, mut refusal: Response) -> Response {
    let mut stream = body.into_data_stream();
    let mut read = 0_u64;
    while let Some(Ok(chunk)) = stream.next().await {
        read += chunk.len() as u64;
        if read > limit {
            refusal
                .headers_mut()
                .insert(header::CONNECTION, HeaderValue::from_static("close"));
            break;
        }
    }
    refusal
}

fn invalid() -> Response {
    ApiError::unauthenticated("CREDENTIALS_INVALID", "The credentials are not valid.")
        .into_response()
}
