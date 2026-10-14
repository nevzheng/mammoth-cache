//! AIP-193 error responses.
//!
//! Every error response, including on Gradle routes, is a `google.rpc.Status` rendered as JSON with
//! one `ErrorInfo` in domain `mammoth-cache.dev` (`api/openapi/common.yaml`).

use std::collections::BTreeMap;

use axum::{
    Json,
    http::{HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;

use crate::{
    auth::Scope,
    cache::{self, Code, Detail},
    key::{CacheKey, InvalidKey, Namespace},
};

/// The `ErrorInfo.domain` of every error this server returns.
pub const ERROR_DOMAIN: &str = "mammoth-cache.dev";

const ERROR_INFO_TYPE: &str = "type.googleapis.com/google.rpc.ErrorInfo";

/// The challenge sent with every `401` (`api/openapi/common.yaml`).
pub const CHALLENGE: HeaderValue = HeaderValue::from_static(r#"Basic realm="mammoth-cache""#);

/// An error response in the shape of AIP-193.
#[derive(Debug, Clone)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    reason: &'static str,
    message: String,
    metadata: BTreeMap<&'static str, String>,
    challenge: bool,
}

impl ApiError {
    /// Creates an error with an HTTP status, canonical code name and `ErrorInfo.reason`.
    pub fn new(
        status: StatusCode,
        code: &'static str,
        reason: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            status,
            code,
            reason,
            message: message.into(),
            metadata: BTreeMap::new(),
            challenge: false,
        }
    }

    /// Adds an `ErrorInfo.metadata` entry.
    #[must_use]
    pub fn with(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.metadata.insert(key, value.into());
        self
    }

    /// The HTTP status.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// The `ErrorInfo.reason`.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        self.reason
    }

    /// `404 NOT_FOUND` for a missing cache entry. Not an error for any client: it builds.
    #[must_use]
    pub fn entry_not_found(key: &CacheKey) -> Self {
        let field = match key.namespace() {
            Namespace::Gradle => "key",
            Namespace::Maven => "path",
            Namespace::BazelAc | Namespace::BazelCas => "digest",
        };
        Self::new(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "ENTRY_NOT_FOUND",
            "Cache entry not found.",
        )
        .with(field, key.name())
    }

    /// `401 UNAUTHENTICATED`, with the `WWW-Authenticate` challenge every `401` carries: Maven
    /// sends credentials on `GET` only after it. `reason` is the AIP-193 reason, such as
    /// `CREDENTIALS_MISSING`.
    #[must_use]
    pub fn unauthenticated(reason: &'static str, message: impl Into<String>) -> Self {
        let mut error = Self::new(StatusCode::UNAUTHORIZED, "UNAUTHENTICATED", reason, message);
        error.challenge = true;
        error
    }

    /// `403 PERMISSION_DENIED`: the user is known but lacks `scope`.
    #[must_use]
    pub fn permission_denied(user: &str, scope: Scope) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "PERMISSION_DENIED",
            "SCOPE_MISSING",
            format!("User {user:?} lacks the {scope} scope."),
        )
        .with("user", user)
        .with("requiredScope", scope.as_str())
    }

    /// `405` for a method the route does not support.
    #[must_use]
    pub fn method_not_allowed(method: &Method) -> Self {
        Self::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "UNIMPLEMENTED",
            "METHOD_NOT_ALLOWED",
            format!("{method} is not supported on this route."),
        )
    }

    /// `404 NOT_FOUND` for an unknown route.
    #[must_use]
    pub fn route_not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "ROUTE_NOT_FOUND",
            "No such route.",
        )
    }

    /// `400 INVALID_ARGUMENT` for a Bazel CAS upload whose SHA-256 is not its digest.
    #[must_use]
    pub fn digest_mismatch(key: &CacheKey) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "INVALID_ARGUMENT",
            "DIGEST_MISMATCH",
            "The uploaded content does not hash to its digest.",
        )
        .with("digest", key.name())
    }

    /// `501 UNIMPLEMENTED` for a route in the contract that is not built yet.
    #[must_use]
    pub fn unimplemented(what: &str) -> Self {
        Self::new(
            StatusCode::NOT_IMPLEMENTED,
            "UNIMPLEMENTED",
            "NOT_IMPLEMENTED",
            format!("{what} is not implemented yet."),
        )
    }
}

impl From<InvalidKey> for ApiError {
    fn from(e: InvalidKey) -> Self {
        let reason = match e {
            InvalidKey::Gradle => "INVALID_CACHE_KEY",
            InvalidKey::Maven => "INVALID_CACHE_PATH",
            InvalidKey::Bazel => "INVALID_DIGEST",
        };
        Self::new(
            StatusCode::BAD_REQUEST,
            "INVALID_ARGUMENT",
            reason,
            sentence(&e),
        )
    }
}

impl From<cache::Error> for ApiError {
    fn from(e: cache::Error) -> Self {
        // Maven always; Gradle only under `OversizePolicy::Reject`, since any non-2xx turns
        // Gradle's remote cache off for the rest of the build.
        if let Some(&Detail::TooLarge { size, limit }) = e.detail() {
            let error = Self::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "OUT_OF_RANGE",
                "ENTRY_TOO_LARGE",
                sentence(&e),
            )
            .with("maxEntryBytes", limit.to_string());
            return match size {
                Some(size) => error.with("entryBytes", size.to_string()),
                None => error,
            };
        }
        // The client's fault, and usually the client is gone: not worth an error log.
        if e.is_upload() {
            tracing::warn!(error = %e, "upload failed");
            return Self::new(
                StatusCode::BAD_REQUEST,
                "INVALID_ARGUMENT",
                "UPLOAD_INCOMPLETE",
                "The request body ended before it was complete.",
            );
        }
        match e.code() {
            Code::Unavailable => {
                tracing::warn!(error = %e, "storage unavailable");
                Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "UNAVAILABLE",
                    "STORAGE_UNAVAILABLE",
                    "Storage is unavailable.",
                )
            }
            // The core sends no other code today; the catalogue (design doc 0017, step 3) maps
            // each one.
            Code::Cancelled
            | Code::Unknown
            | Code::InvalidArgument
            | Code::DeadlineExceeded
            | Code::NotFound
            | Code::AlreadyExists
            | Code::PermissionDenied
            | Code::ResourceExhausted
            | Code::FailedPrecondition
            | Code::Aborted
            | Code::OutOfRange
            | Code::Unimplemented
            | Code::Internal
            | Code::DataLoss
            | Code::Unauthenticated => {
                tracing::error!(error = %e, "storage error");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL",
                    "INTERNAL",
                    "Internal error.",
                )
            }
        }
    }
}

/// Renders a Rust error message (lowercase, no full stop) as an API message.
fn sentence(e: &impl std::fmt::Display) -> String {
    let text = e.to_string();
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        format!("{}{}.", first.to_uppercase(), chars.as_str())
    })
}

#[derive(Serialize)]
struct Body<'a> {
    error: RpcStatus<'a>,
}

#[derive(Serialize)]
struct RpcStatus<'a> {
    code: u16,
    message: &'a str,
    status: &'a str,
    details: [ErrorInfo<'a>; 1],
}

#[derive(Serialize)]
struct ErrorInfo<'a> {
    #[serde(rename = "@type")]
    type_url: &'a str,
    reason: &'a str,
    domain: &'a str,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    metadata: &'a BTreeMap<&'static str, String>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Body {
            error: RpcStatus {
                code: self.status.as_u16(),
                message: &self.message,
                status: self.code,
                details: [ErrorInfo {
                    type_url: ERROR_INFO_TYPE,
                    reason: self.reason,
                    domain: ERROR_DOMAIN,
                    metadata: &self.metadata,
                }],
            },
        };
        let mut response = (self.status, Json(body)).into_response();
        if self.challenge {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, CHALLENGE);
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_401_carries_a_challenge() {
        let response =
            ApiError::unauthenticated("CREDENTIALS_MISSING", "No credentials.").into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers()[header::WWW_AUTHENTICATE],
            r#"Basic realm="mammoth-cache""#
        );
    }
}
