//! AIP-193 error responses.
//!
//! Every error response, including on Gradle routes, is a `google.rpc.Status` rendered as JSON with
//! one `ErrorInfo` in domain `mammoth-cache.dev` (`api/openapi/common.yaml`). The reasons are the
//! catalogue in [`crate::api::error`]; this module picks each one's HTTP status.

use std::collections::BTreeMap;

use axum::{
    Json,
    http::{HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;

use crate::{
    api::error::{Code, DOMAIN, Reason, Status, sentence},
    auth::Scope,
    cache,
    key::{CacheKey, InvalidKey, Namespace},
};

const ERROR_INFO_TYPE: &str = "type.googleapis.com/google.rpc.ErrorInfo";

/// The challenge sent with every `401` (`api/openapi/common.yaml`).
pub const CHALLENGE: HeaderValue = HeaderValue::from_static(r#"Basic realm="mammoth-cache""#);

/// The HTTP status of `reason` on the Gradle, Maven, Bazel and ops routes: AIP-193's mapping of its
/// code, except where these protocols' clients need another (design doc 0017). The three cache
/// protocols want the same overrides today; one that needs different ones gets its own function.
#[must_use]
pub fn http_status(reason: Reason) -> StatusCode {
    match reason {
        // Maven and Bazel log a 413 and carry on; Gradle sees it only under the Reject policy.
        Reason::EntryTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        // Every failed upload has been 400 on HTTP; AIP-193 would send 499 and 504.
        Reason::UploadCancelled | Reason::UploadIdle => StatusCode::BAD_REQUEST,
        // HTTP semantics, with `Allow`.
        Reason::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
        Reason::EntryNotFound
        | Reason::RouteNotFound
        | Reason::InvalidRequest
        | Reason::DigestMismatch
        | Reason::SizeMismatch
        | Reason::UploadIncomplete
        | Reason::DownloadStalled
        | Reason::EmptyWrite
        | Reason::UnsupportedCompressor
        | Reason::ActionResultMissing
        | Reason::InvalidCacheKey
        | Reason::InvalidCachePath
        | Reason::InvalidDigest
        | Reason::InvalidResourceName
        | Reason::InvalidReadLimit
        | Reason::InvalidWrite
        | Reason::OffsetOutOfRange
        | Reason::BatchTooLarge
        | Reason::TooManyDigests
        | Reason::MessageTooLarge
        | Reason::UnsupportedDigestFunction
        | Reason::CredentialsMissing
        | Reason::CredentialsInvalid
        | Reason::ScopeMissing
        | Reason::MethodNotSupported
        | Reason::StorageUnavailable
        | Reason::StorageNotWritable
        | Reason::StorageFreeSpaceLow
        | Reason::ShuttingDown
        | Reason::Internal => aip_status(reason.code()),
    }
}

/// AIP-193's HTTP status for a canonical code (`google.rpc.Code`'s mapping).
#[must_use]
pub fn aip_status(code: Code) -> StatusCode {
    match code {
        Code::InvalidArgument | Code::FailedPrecondition | Code::OutOfRange => {
            StatusCode::BAD_REQUEST
        }
        Code::Unauthenticated => StatusCode::UNAUTHORIZED,
        Code::PermissionDenied => StatusCode::FORBIDDEN,
        Code::NotFound => StatusCode::NOT_FOUND,
        Code::Aborted | Code::AlreadyExists => StatusCode::CONFLICT,
        Code::ResourceExhausted => StatusCode::TOO_MANY_REQUESTS,
        // 499 Client Closed Request: not in the standard registry, but always valid.
        Code::Cancelled => StatusCode::from_u16(499).unwrap_or(StatusCode::BAD_REQUEST),
        Code::Unknown | Code::Internal | Code::DataLoss => StatusCode::INTERNAL_SERVER_ERROR,
        Code::Unimplemented => StatusCode::NOT_IMPLEMENTED,
        Code::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        Code::DeadlineExceeded => StatusCode::GATEWAY_TIMEOUT,
    }
}

/// An error response in the shape of AIP-193: a catalogue [`Status`] at its HTTP status.
#[derive(Debug, Clone)]
pub struct ApiError {
    status: Status,
    http: StatusCode,
    challenge: bool,
}

impl ApiError {
    /// An error for `reason`, at the status [`http_status`] gives it.
    pub fn new(reason: Reason, message: impl Into<std::borrow::Cow<'static, str>>) -> Self {
        Status::new(reason, message).into()
    }

    /// Adds an `ErrorInfo.metadata` entry.
    #[must_use]
    pub fn with(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.status = self.status.with(key, value);
        self
    }

    /// The HTTP status.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.http
    }

    /// The `ErrorInfo.reason`.
    #[must_use]
    pub fn reason(&self) -> Reason {
        self.status.reason()
    }

    /// `404 NOT_FOUND` for a missing cache entry. Not an error for any client: it builds.
    #[must_use]
    pub fn entry_not_found(key: &CacheKey) -> Self {
        let field = match key.namespace() {
            Namespace::Gradle => "key",
            Namespace::Maven => "path",
            Namespace::BazelAc | Namespace::BazelCas => "digest",
        };
        Self::new(Reason::EntryNotFound, "Cache entry not found.").with(field, key.name())
    }

    /// `401 UNAUTHENTICATED`, with the `WWW-Authenticate` challenge every `401` carries: Maven
    /// sends credentials on `GET` only after it.
    #[must_use]
    pub fn unauthenticated(
        reason: Reason,
        message: impl Into<std::borrow::Cow<'static, str>>,
    ) -> Self {
        let mut error = Self::new(reason, message);
        error.challenge = true;
        error
    }

    /// `403 PERMISSION_DENIED`: the user is known but lacks `scope`.
    #[must_use]
    pub fn permission_denied(user: &str, scope: Scope) -> Self {
        Self::new(
            Reason::ScopeMissing,
            format!("User {user:?} lacks the {scope} scope."),
        )
        .with("user", user)
        .with("requiredScope", scope.as_str())
    }

    /// `405` for a method the route does not support.
    #[must_use]
    pub fn method_not_allowed(method: &Method) -> Self {
        Self::new(
            Reason::MethodNotAllowed,
            format!("{method} is not supported on this route."),
        )
    }

    /// `404 NOT_FOUND` for an unknown route.
    #[must_use]
    pub fn route_not_found() -> Self {
        Self::new(Reason::RouteNotFound, "No such route.")
    }

    /// `400 INVALID_ARGUMENT` for a Bazel CAS upload whose SHA-256 is not its digest.
    #[must_use]
    pub fn digest_mismatch(key: &CacheKey) -> Self {
        Self::new(
            Reason::DigestMismatch,
            "The uploaded content does not hash to its digest.",
        )
        .with("key", key.name())
    }

    /// `501 UNIMPLEMENTED` for `method`, which this server does not offer, such as `GET /metrics`
    /// with metrics off.
    #[must_use]
    pub fn method_not_supported(
        method: &str,
        message: impl Into<std::borrow::Cow<'static, str>>,
    ) -> Self {
        Self::new(Reason::MethodNotSupported, message).with("method", method)
    }
}

impl From<Status> for ApiError {
    fn from(status: Status) -> Self {
        Self {
            http: http_status(status.reason()),
            status,
            challenge: false,
        }
    }
}

impl From<InvalidKey> for ApiError {
    fn from(e: InvalidKey) -> Self {
        let reason = match e {
            InvalidKey::Gradle => Reason::InvalidCacheKey,
            InvalidKey::Maven => Reason::InvalidCachePath,
            InvalidKey::Bazel => Reason::InvalidDigest,
        };
        Self::new(reason, sentence(&e))
    }
}

impl From<cache::Error> for ApiError {
    fn from(e: cache::Error) -> Self {
        let status = Status::from(&e);
        match status.code() {
            // The client's fault, and usually the client is gone: not worth an error log.
            Code::InvalidArgument | Code::Cancelled | Code::DeadlineExceeded
                if status.reason() != Reason::EntryTooLarge =>
            {
                tracing::warn!(error = %e, "upload failed");
            }
            Code::Unavailable => tracing::warn!(error = %e, "storage unavailable"),
            Code::Internal => tracing::error!(error = %e, "storage error"),
            _ => {}
        }
        status.into()
    }
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
    metadata: &'a BTreeMap<&'a str, &'a str>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let metadata: BTreeMap<&str, &str> = self
            .status
            .metadata()
            .iter()
            .map(|(key, value)| (*key, value.as_str()))
            .collect();
        let body = Body {
            error: RpcStatus {
                code: self.http.as_u16(),
                message: self.status.message(),
                status: self.status.code().as_str(),
                details: [ErrorInfo {
                    type_url: ERROR_INFO_TYPE,
                    reason: self.status.reason().as_str(),
                    domain: DOMAIN,
                    metadata: &metadata,
                }],
            },
        };
        let mut response = (self.http, Json(body)).into_response();
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

    #[tokio::test]
    async fn every_reason_renders_as_aip_193() {
        for &reason in Reason::ALL {
            let response = ApiError::new(reason, "Message.")
                .with("k", "v")
                .into_response();
            assert_eq!(response.status(), http_status(reason), "{reason:?}");
            let body = axum::body::to_bytes(response.into_body(), 1 << 16).await;
            let json: serde_json::Value = serde_json::from_slice(&body.unwrap()).unwrap();
            let error = &json["error"];
            assert_eq!(error["code"], http_status(reason).as_u16(), "{reason:?}");
            assert_eq!(error["status"], reason.code().as_str(), "{reason:?}");
            assert_eq!(error["message"], "Message.");
            let info = &error["details"][0];
            assert_eq!(info["@type"], ERROR_INFO_TYPE);
            assert_eq!(info["reason"], reason.as_str());
            assert_eq!(info["domain"], DOMAIN);
            assert_eq!(info["metadata"]["k"], "v");
        }
    }

    #[test]
    fn every_401_carries_a_challenge() {
        let response = ApiError::unauthenticated(Reason::CredentialsMissing, "No credentials.")
            .into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers()[header::WWW_AUTHENTICATE],
            r#"Basic realm="mammoth-cache""#
        );
    }
}
