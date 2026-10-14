//! Inbound HTTP adapters: translate HTTP to core calls, and core errors to AIP-193 responses.

mod admin;
mod entries;
pub mod error;
mod gradle;
mod maven;
pub mod metrics;
mod ops;

use std::sync::Arc;

use axum::{Router, extract::DefaultBodyLimit, middleware, routing::get};
use metrics_exporter_prometheus::PrometheusHandle;

use crate::cache::Cache;
use error::ApiError;

/// State shared by every handler.
#[derive(Debug, Clone)]
pub struct AppState {
    /// The cache core.
    pub cache: Arc<Cache>,
    /// Renders `/metrics`, when a Prometheus recorder is installed.
    pub metrics: Option<PrometheusHandle>,
}

/// Builds the HTTP router over a cache. `metrics` renders `/metrics`; without it, `/metrics`
/// answers `501`.
pub fn router(cache: Arc<Cache>, metrics: Option<PrometheusHandle>) -> Router {
    let entries = Router::new()
        .route(
            "/cache/{key}",
            get(gradle::load_entry)
                .head(gradle::check_entry)
                .put(gradle::store_entry),
        )
        .route(
            "/maven/{*path}",
            get(maven::load_object)
                .head(maven::check_object)
                .put(maven::store_object)
                .options(maven::describe_object)
                .fallback(maven::other_method),
        )
        // Entry bodies are streamed and the core enforces the entry size limit.
        .layer(DefaultBodyLimit::disable());

    Router::new()
        .merge(entries)
        .route("/ping", get(ops::ping))
        .route("/livez", get(ops::livez))
        .route("/readyz", get(ops::readyz))
        .route("/metrics", get(ops::metrics))
        .route("/v1/serverStatus", get(admin::get_server_status))
        .fallback(|| async { ApiError::route_not_found() })
        .layer(middleware::from_fn(metrics::record))
        .with_state(AppState { cache, metrics })
}
