//! Authentication for the gRPC services: the same users, tokens and scopes as the HTTP routes
//! (design doc 0002). The token comes in the `authorization` metadata, as `Bearer <token>` or as
//! HTTP Basic with the token as the password.

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use tonic::{Code, Status};

use super::status;
use crate::auth::{Authenticator, Credentials, Scope};

/// The scope a gRPC method needs, from its path. `None` for anything else, which the router
/// answers `UNIMPLEMENTED`.
fn required_scope(path: &str) -> Option<Scope> {
    let (service, method) = path.strip_prefix('/')?.split_once('/')?;
    let read = match (service, method) {
        ("build.bazel.remote.execution.v2.Capabilities", "GetCapabilities")
        | ("build.bazel.remote.execution.v2.ActionCache", "GetActionResult")
        | (
            "build.bazel.remote.execution.v2.ContentAddressableStorage",
            "FindMissingBlobs" | "BatchReadBlobs" | "GetTree" | "SplitBlob",
        )
        | ("google.bytestream.ByteStream", "Read" | "QueryWriteStatus") => true,
        ("build.bazel.remote.execution.v2.ActionCache", "UpdateActionResult")
        | (
            "build.bazel.remote.execution.v2.ContentAddressableStorage",
            "BatchUpdateBlobs" | "SpliceBlob",
        )
        | ("google.bytestream.ByteStream", "Write") => false,
        _ => return None,
    };
    Some(if read {
        Scope::CacheRead
    } else {
        Scope::CacheWrite
    })
}

/// Middleware that checks each call's credentials against the scope its method needs.
pub(crate) async fn require(
    State(auth): State<Arc<Authenticator>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(scope) = required_scope(request.uri().path()) else {
        return next.run(request).await;
    };
    let header = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .map(|v| v.to_str().map(str::to_owned));
    let presented = match header {
        None => None,
        Some(Ok(value)) => match parse(&value) {
            Some(parsed) => Some(parsed),
            None => return refuse(CREDENTIALS_INVALID, &invalid()),
        },
        Some(Err(_)) => return refuse(CREDENTIALS_INVALID, &invalid()),
    };
    let credentials = presented.as_ref().map(|(user, token)| match user {
        Some(user) => Credentials::Basic { user, token },
        None => Credentials::Bearer { token },
    });
    let Ok(principal) = auth.authenticate(credentials) else {
        return refuse(CREDENTIALS_INVALID, &invalid());
    };
    if principal.scopes.contains(scope) {
        return next.run(request).await;
    }
    let (reason, status) = match &principal.user {
        None => (
            CREDENTIALS_MISSING,
            status::error(
                Code::Unauthenticated,
                CREDENTIALS_MISSING,
                "Authentication is required.",
                &[],
            ),
        ),
        Some(user) => (
            SCOPE_MISSING,
            status::error(
                Code::PermissionDenied,
                SCOPE_MISSING,
                format!("User {user:?} lacks the {scope} scope."),
                &[("user", user), ("requiredScope", scope.as_str())],
            ),
        ),
    };
    refuse(reason, &status)
}

/// Parses `Bearer <token>` or `Basic <base64 user:token>`: the user, if any, and the token.
fn parse(value: &str) -> Option<(Option<String>, String)> {
    let (scheme, rest) = value.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") {
        Some((None, rest.trim().to_owned()))
    } else if scheme.eq_ignore_ascii_case("basic") {
        let decoded = String::from_utf8(STANDARD.decode(rest.trim()).ok()?).ok()?;
        let (user, token) = decoded.split_once(':')?;
        Some((Some(user.to_owned()), token.to_owned()))
    } else {
        None
    }
}

const CREDENTIALS_MISSING: &str = "CREDENTIALS_MISSING";
const CREDENTIALS_INVALID: &str = "CREDENTIALS_INVALID";
const SCOPE_MISSING: &str = "SCOPE_MISSING";

fn invalid() -> Status {
    status::error(
        Code::Unauthenticated,
        CREDENTIALS_INVALID,
        "The credentials are not valid.",
        &[],
    )
}

/// A trailers-only gRPC response carrying `status`, counted in `mammoth_auth_failures_total` by
/// its `reason`, as the HTTP adapters count theirs.
fn refuse(reason: &'static str, status: &Status) -> Response {
    metrics::counter!("mammoth_auth_failures_total", "reason" => reason).increment(1);
    status
        .clone()
        .into_http::<axum::body::Body>()
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_by_method() {
        let scope = |p| required_scope(p);
        assert_eq!(
            scope("/build.bazel.remote.execution.v2.Capabilities/GetCapabilities"),
            Some(Scope::CacheRead)
        );
        assert_eq!(
            scope("/build.bazel.remote.execution.v2.ActionCache/UpdateActionResult"),
            Some(Scope::CacheWrite)
        );
        assert_eq!(
            scope("/build.bazel.remote.execution.v2.ContentAddressableStorage/FindMissingBlobs"),
            Some(Scope::CacheRead)
        );
        assert_eq!(
            scope("/build.bazel.remote.execution.v2.ContentAddressableStorage/BatchUpdateBlobs"),
            Some(Scope::CacheWrite)
        );
        assert_eq!(
            scope("/google.bytestream.ByteStream/Write"),
            Some(Scope::CacheWrite)
        );
        assert_eq!(
            scope("/google.bytestream.ByteStream/Read"),
            Some(Scope::CacheRead)
        );
        assert_eq!(
            scope("/build.bazel.remote.execution.v2.Execution/Execute"),
            None
        );
        assert_eq!(scope("/nonsense"), None);
    }

    #[test]
    fn parses_credentials() {
        assert_eq!(parse("Bearer abc"), Some((None, "abc".to_owned())));
        assert_eq!(
            parse(&format!("Basic {}", STANDARD.encode("ci:tok"))),
            Some((Some("ci".to_owned()), "tok".to_owned()))
        );
        assert_eq!(parse("Digest x"), None);
        assert_eq!(parse("Basic !!!"), None);
    }
}
