//! mammoth-cache: a remote build cache node for Gradle.
//!
//! Pre-alpha. The HTTP surface follows Gradle's HTTP build cache protocol; only `/ping` works so far.

use axum::{Router, http::StatusCode, routing::get};

/// Builds the HTTP router.
pub fn router() -> Router {
    Router::new().route("/ping", get(ping)).route(
        "/cache/{key}",
        get(not_implemented)
            .put(not_implemented)
            .head(not_implemented),
    )
}

async fn ping() -> &'static str {
    "SUCCESS"
}

async fn not_implemented() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

#[cfg(test)]
mod tests {
    use super::router;
    use axum::{body::Body, http::Request, http::StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn ping_returns_success() {
        let response = router()
            .oneshot(Request::get("/ping").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"SUCCESS");
    }
}
