//! Operational routes: health checks and metrics (`api/openapi/ops.v1.yaml`). No authentication.

use axum::extract::Query;
use serde::Deserialize;

use super::error::ApiError;

/// Build Cache Node compatible health check.
pub(super) async fn ping() -> &'static str {
    "SUCCESS"
}

/// Liveness: the process answers. Never checks storage, so a full disk never restarts the process.
pub(super) async fn livez() -> &'static str {
    "ok"
}

#[derive(Debug, Deserialize)]
pub(super) struct ReadyzParams {
    verbose: Option<String>,
}

/// Readiness. Storage checks (writable, free space above the buffer) arrive with the disk tier;
/// until then the only check is that configuration loaded, which is true once serving.
pub(super) async fn readyz(Query(params): Query<ReadyzParams>) -> String {
    if params.verbose.is_some() {
        "[+]config ok\nreadyz check passed\n".to_owned()
    } else {
        "ok".to_owned()
    }
}

/// Prometheus metrics. The catalog is design doc 0004; not wired up yet.
pub(super) async fn metrics() -> ApiError {
    ApiError::unimplemented("Metrics")
}
