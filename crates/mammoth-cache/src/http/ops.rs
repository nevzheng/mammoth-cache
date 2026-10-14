//! Operational routes: health checks and metrics (`api/openapi/ops.v1.yaml`). No authentication.

use axum::{
    extract::{Query, State},
    http::{HeaderValue, StatusCode, header},
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

/// Readiness: not shutting down, and every local tier passes its storage checks. The checks are
/// cached results of the tiers' background probes, so a probe costs nothing and a slow disk cannot
/// time it out. The bucket is never checked.
pub(super) async fn readyz(
    State(state): State<AppState>,
    Query(params): Query<ReadyzParams>,
) -> Result<String, ApiError> {
    let mut checks = vec![("config", None)];
    let shutting_down = state.cache.is_shutting_down().then(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "UNAVAILABLE",
            "SHUTTING_DOWN",
            "The node is shutting down.",
        )
        .with("check", "shutting-down")
    });
    checks.push(("shutting-down", shutting_down));
    for check in state.cache.health() {
        let error = check.problem.map(|problem| {
            let error = ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "UNAVAILABLE",
                problem.reason,
                problem.message,
            )
            .with("check", check.name);
            problem
                .metadata
                .into_iter()
                .fold(error, |error, (key, value)| error.with(key, value))
        });
        checks.push((check.name, error));
    }
    if let Some((_, Some(error))) = checks.iter_mut().find(|(_, error)| error.is_some()) {
        return Err(error.clone());
    }
    if params.verbose.is_none() {
        return Ok("ok".to_owned());
    }
    let mut out = String::new();
    for (name, _) in &checks {
        out.push_str("[+]");
        out.push_str(name);
        out.push_str(" ok\n");
    }
    out.push_str("readyz check passed\n");
    Ok(out)
}

/// Prometheus metrics in the text exposition format. The catalog is design doc 0004.
pub(super) async fn metrics(State(state): State<AppState>) -> Result<Response, ApiError> {
    let handle = state
        .metrics
        .as_ref()
        .ok_or_else(|| ApiError::unimplemented("Metrics"))?;
    let headers = [(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4"),
    )];
    Ok((headers, crate::telemetry::render(handle)).into_response())
}
