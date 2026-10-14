//! The Apache Maven Build Cache Extension protocol: `/maven/{path}`
//! (`api/openapi/maven-build-cache.v1.yaml`).
//!
//! The extension stores named files under deep paths, so the whole path is the key, in a namespace
//! of its own. A store replaces what is at the path, and an oversized store gets an honest `413`:
//! Maven only logs a failed store and keeps using the cache.

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};

use super::{AppState, entries, error::ApiError, name::Name};
use crate::key::CacheKey;

const ALLOW: HeaderValue = HeaderValue::from_static("GET, HEAD, PUT, OPTIONS");

pub(super) async fn load_object(
    State(state): State<AppState>,
    Name(path): Name,
) -> Result<Response, ApiError> {
    entries::load(&state, &CacheKey::maven(&path)?).await
}

pub(super) async fn check_object(
    State(state): State<AppState>,
    Name(path): Name,
) -> Result<Response, ApiError> {
    entries::check(&state, &CacheKey::maven(&path)?).await
}

pub(super) async fn store_object(
    State(state): State<AppState>,
    Name(path): Name,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, ApiError> {
    entries::store(&state, &CacheKey::maven(&path)?, &headers, body).await
}

/// `OPTIONS`, sent when the client has `WebDAV` on. No `DAV` header, so the client skips `MKCOL`.
pub(super) async fn describe_object(Name(path): Name) -> Result<Response, ApiError> {
    CacheKey::maven(&path)?;
    Ok((StatusCode::NO_CONTENT, [(header::ALLOW, ALLOW)]).into_response())
}

/// Every other method. A stray `WebDAV` `MKCOL` gets `201` and does nothing, because directories
/// are implicit; anything else is `405`.
pub(super) async fn other_method(method: Method, Name(path): Name) -> Response {
    if let Err(e) = CacheKey::maven(&path) {
        return ApiError::from(e).into_response();
    }
    if method.as_str() == "MKCOL" {
        return StatusCode::CREATED.into_response();
    }
    let mut response = ApiError::method_not_allowed(&method).into_response();
    response.headers_mut().insert(header::ALLOW, ALLOW);
    response
}
