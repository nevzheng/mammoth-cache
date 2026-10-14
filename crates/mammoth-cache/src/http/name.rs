//! The entry name in a cache route's path. axum rejects a path it cannot decode, such as
//! `/cache/%FF`, with a plain-text `400`; this extractor answers it as the protocol's own invalid
//! key instead.

use axum::{
    extract::{FromRequestParts, Path},
    http::request::Parts,
};

use super::error::ApiError;
use crate::key::InvalidKey;

/// The `{key}`, `{*path}` or `{digest}` of a Gradle, Maven or Bazel route, as sent.
pub(super) struct Name(pub(super) String);

impl<S: Send + Sync> FromRequestParts<S> for Name {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        match Path::<String>::from_request_parts(parts, state).await {
            Ok(Path(name)) => Ok(Self(name)),
            Err(_) => Err(invalid(parts.uri.path()).into()),
        }
    }
}

/// The invalid-key error of the protocol that serves `path`.
fn invalid(path: &str) -> InvalidKey {
    if path.starts_with("/maven/") {
        InvalidKey::Maven
    } else if path.starts_with("/bazel/") {
        InvalidKey::Bazel
    } else {
        InvalidKey::Gradle
    }
}
