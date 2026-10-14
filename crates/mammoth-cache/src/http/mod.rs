//! Inbound HTTP adapters: translate HTTP to core calls, and core errors to AIP-193 responses.

mod admin;
mod auth;
mod bazel;
mod entries;
pub mod error;
mod gradle;
mod maven;
pub mod metrics;
mod ops;

use std::sync::Arc;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::{HeaderValue, Method, header},
    middleware,
    response::{IntoResponse, Response},
    routing::get,
};
use metrics_exporter_prometheus::PrometheusHandle;

use crate::{auth::Authenticator, cache::Cache, config::Protocols};
use error::ApiError;

/// State shared by every handler.
#[derive(Debug, Clone)]
pub struct AppState {
    /// The cache core.
    pub cache: Arc<Cache>,
    /// Renders `/metrics`, when a Prometheus recorder is installed.
    pub metrics: Option<PrometheusHandle>,
}

/// Builds the HTTP router over a cache, open to everyone, serving the default protocols.
/// `metrics` renders `/metrics`; without it, `/metrics` answers `501`.
pub fn router(cache: Arc<Cache>, metrics: Option<PrometheusHandle>) -> Router {
    router_with_auth(cache, metrics, Arc::new(Authenticator::open()))
}

/// Builds the HTTP router over a cache, serving the default protocols and checking every request
/// against `auth`.
pub fn router_with_auth(
    cache: Arc<Cache>,
    metrics: Option<PrometheusHandle>,
    auth: Arc<Authenticator>,
) -> Router {
    router_with(cache, metrics, auth, Protocols::default())
}

/// Builds the HTTP router over a cache, checking every request against `auth`. Only the
/// `protocols` that are on get routes; requests for the others get the `404` of an unknown path.
pub fn router_with(
    cache: Arc<Cache>,
    metrics: Option<PrometheusHandle>,
    auth: Arc<Authenticator>,
    protocols: Protocols,
) -> Router {
    let mut entries = Router::new();
    if protocols.gradle {
        entries = entries.route(
            "/cache/{key}",
            get(gradle::load_entry)
                .head(gradle::check_entry)
                .put(gradle::store_entry)
                .fallback(async |method: Method| method_not_allowed(&method, "GET, HEAD, PUT")),
        );
    }
    if protocols.maven {
        entries = entries.route(
            "/maven/{*path}",
            get(maven::load_object)
                .head(maven::check_object)
                .put(maven::store_object)
                .options(maven::describe_object)
                .fallback(maven::other_method),
        );
    }
    if protocols.bazel {
        entries = entries
            .route(
                "/bazel/ac/{digest}",
                get(bazel::load_action)
                    .head(bazel::check_action)
                    .put(bazel::store_action)
                    .fallback(async |method: Method| method_not_allowed(&method, "GET, HEAD, PUT")),
            )
            .route(
                "/bazel/cas/{digest}",
                get(bazel::load_blob)
                    .head(bazel::check_blob)
                    .put(bazel::store_blob)
                    .fallback(async |method: Method| method_not_allowed(&method, "GET, HEAD, PUT")),
            );
    }
    // Entry bodies are streamed and the core enforces the entry size limit.
    let entries = entries.layer(DefaultBodyLimit::disable());

    Router::new()
        .merge(entries)
        .route("/ping", get(ops::ping))
        .route("/livez", get(ops::livez))
        .route("/readyz", get(ops::readyz))
        .route("/metrics", get(ops::metrics))
        .route("/v1/serverStatus", get(admin::get_server_status))
        .fallback(|| async { ApiError::route_not_found() })
        .layer(middleware::from_fn_with_state(
            auth::Guard {
                auth,
                protocols,
                drain_limit: cache.max_entry_size(),
            },
            auth::require,
        ))
        .layer(middleware::from_fn(error::render))
        .layer(middleware::from_fn(metrics::record))
        .layer(middleware::from_fn(crate::access::http))
        .with_state(AppState { cache, metrics })
}

/// An AIP-193 `405` with the route's `Allow` header, in place of axum's empty one.
fn method_not_allowed(method: &Method, allow: &'static str) -> Response {
    let mut response = ApiError::method_not_allowed(method).into_response();
    response
        .headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static(allow));
    response
}
