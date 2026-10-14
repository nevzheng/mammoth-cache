//! AIP-193 error responses.
//!
//! Every error response, including on Gradle routes, is a `google.rpc.Status` rendered as JSON with
//! one `ErrorInfo` in domain `mammoth-cache.dev` (`api/openapi/common.yaml`).

use std::collections::BTreeMap;

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;

use crate::{cache::CacheError, key::InvalidKey};

/// The `ErrorInfo.domain` of every error this server returns.
pub const ERROR_DOMAIN: &str = "mammoth-cache.dev";

const ERROR_INFO_TYPE: &str = "type.googleapis.com/google.rpc.ErrorInfo";

/// An error response in the shape of AIP-193.
#[derive(Debug, Clone)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    reason: &'static str,
    message: String,
    metadata: BTreeMap<&'static str, String>,
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

    /// `404 NOT_FOUND` for a missing cache entry. Not an error for Gradle; it runs the task.
    #[must_use]
    pub fn entry_not_found(key: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "ENTRY_NOT_FOUND",
            "Cache entry not found.",
        )
        .with("key", key)
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
        Self::new(
            StatusCode::BAD_REQUEST,
            "INVALID_ARGUMENT",
            "INVALID_CACHE_KEY",
            sentence(&e),
        )
    }
}

impl From<CacheError> for ApiError {
    fn from(e: CacheError) -> Self {
        match e {
            // Only under `OversizePolicy::Reject`. Any non-2xx turns Gradle's remote cache off for
            // the rest of the build; 413 at least keeps the Build Cache Node's meaning.
            CacheError::TooLarge { size, limit } => {
                let error = Self::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "OUT_OF_RANGE",
                    "ENTRY_TOO_LARGE",
                    sentence(&e),
                )
                .with("maxEntryBytes", limit.to_string());
                match size {
                    Some(size) => error.with("entryBytes", size.to_string()),
                    None => error,
                }
            }
            CacheError::Unavailable(ref source) => {
                tracing::warn!(error = %source, "storage unavailable");
                Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "UNAVAILABLE",
                    "STORAGE_UNAVAILABLE",
                    "Storage is unavailable.",
                )
            }
            CacheError::Internal(ref source) => {
                tracing::error!(error = %source, "storage error");
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
        (self.status, Json(body)).into_response()
    }
}
