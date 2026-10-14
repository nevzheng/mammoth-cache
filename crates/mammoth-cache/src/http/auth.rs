//! Authentication and authorization for every route (design doc 0002).
//!
//! Each route needs one scope. The middleware reads the `Authorization` header (Basic with the
//! token as the password, or Bearer), finds the principal and either forwards the request, with
//! the [`Principal`] as an extension, or answers `401` or `403`. It runs before the handler reads
//! the body, so a refused upload with `Expect: 100-continue` is never sent.

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{HeaderMap, Method, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};

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
    let Ok(presented) = credentials(request.headers()) else {
        return invalid();
    };
    let creds = presented.as_ref().map(|(user, token)| {
        if user.is_empty() {
            Credentials::Bearer { token }
        } else {
            Credentials::Basic { user, token }
        }
    });
    let Ok(principal) = auth.authenticate(creds) else {
        return invalid();
    };
    if principal.scopes.contains(scope) {
        request.extensions_mut().insert(principal);
        return next.run(request).await;
    }
    match &principal.user {
        None => ApiError::unauthenticated("CREDENTIALS_MISSING", "Authentication is required.")
            .into_response(),
        Some(user) => ApiError::permission_denied(user, scope).into_response(),
    }
}

fn invalid() -> Response {
    ApiError::unauthenticated("CREDENTIALS_INVALID", "The credentials are not valid.")
        .into_response()
}
