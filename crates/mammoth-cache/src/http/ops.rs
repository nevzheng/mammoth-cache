//! Operational routes: health checks and metrics (`api/openapi/ops.v1.yaml`). No authentication.

use axum::{
    extract::{Query, State},
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;

use super::{AppState, error::ApiError};

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

/// Prometheus metrics in the text exposition format. The catalog is design doc 0004; only the
/// request counters are recorded so far.
pub(super) async fn metrics(State(state): State<AppState>) -> Result<Response, ApiError> {
    let handle = state
        .metrics
        .as_ref()
        .ok_or_else(|| ApiError::unimplemented("Metrics"))?;
    let headers = [(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4"),
    )];
    Ok((headers, handle.render()).into_response())
}
