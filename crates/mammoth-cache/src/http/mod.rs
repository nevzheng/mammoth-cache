//! Inbound HTTP adapters: translate HTTP to core calls, and core errors to AIP-193 responses.

mod admin;
pub mod error;
mod gradle;
mod ops;

use std::sync::Arc;

use axum::{Router, extract::DefaultBodyLimit, routing::get};

use crate::cache::Cache;
use error::ApiError;

/// State shared by every handler.
#[derive(Debug, Clone)]
pub struct AppState {
    /// The cache core.
    pub cache: Arc<Cache>,
}

/// Builds the HTTP router over a cache.
pub fn router(cache: Arc<Cache>) -> Router {
    let gradle = Router::new()
        .route(
            "/cache/{key}",
            get(gradle::load_entry)
                .head(gradle::check_entry)
                .put(gradle::store_entry),
        )
        // Entry bodies are streamed and the core enforces the entry size limit.
        .layer(DefaultBodyLimit::disable());

    Router::new()
        .merge(gradle)
        .route("/ping", get(ops::ping))
        .route("/livez", get(ops::livez))
        .route("/readyz", get(ops::readyz))
        .route("/metrics", get(ops::metrics))
        .route("/v1/serverStatus", get(admin::get_server_status))
        .fallback(|| async { ApiError::route_not_found() })
        .with_state(AppState { cache })
}
