//! AIP-193 error responses.
//!
//! Every error response, including on Gradle routes, is a `google.rpc.Status` rendered as JSON with
//! one `ErrorInfo` in domain `mammoth-cache.dev` (`api/openapi/common.yaml`). The reasons are the
//! catalogue in [`crate::api::error`]; this module picks each one's HTTP status.

use std::{collections::BTreeMap, sync::Arc};

use axum::{
    Json,
    extract::Request,
    http::{HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Serialize;

use crate::{
    api::error::{Code, DOMAIN, Reason, Status, sentence},
    auth::Scope,
    cache,
    key::{CacheKey, InvalidKey, Namespace},
};

const ERROR_INFO_TYPE: &str = "type.googleapis.com/google.rpc.ErrorInfo";

/// The challenge sent with every `401` (`api/openapi/common.yaml`).
pub const CHALLENGE: HeaderValue = HeaderValue::from_static(r#"Basic realm="mammoth-cache""#);

/// The HTTP status of `reason` on the Gradle, Maven, Bazel and ops routes: AIP-193's mapping of its
/// code, except where these protocols' clients need another (design doc 0017). The three cache
/// protocols want the same overrides today; one that needs different ones gets its own function.
#[must_use]
pub fn http_status(reason: Reason) -> StatusCode {
    match reason {
        // Maven and Bazel log a 413 and carry on; Gradle sees it only under the Reject policy.
        Reason::EntryTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        // Every failed upload has been 400 on HTTP; AIP-193 would send 499 and 504.
        Reason::UploadCancelled | Reason::UploadIdle => StatusCode::BAD_REQUEST,
        // HTTP semantics, with `Allow`.
        Reason::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
        Reason::EntryNotFound
        | Reason::RouteNotFound
        | Reason::InvalidRequest
        | Reason::DigestMismatch
        | Reason::SizeMismatch
        | Reason::UploadIncomplete
        | Reason::DownloadStalled
        | Reason::EmptyWrite
        | Reason::UnsupportedCompressor
        | Reason::ActionResultMissing
        | Reason::InvalidCacheKey
        | Reason::InvalidCachePath
        | Reason::InvalidDigest
        | Reason::InvalidResourceName
        | Reason::InvalidReadLimit
        | Reason::InvalidWrite
        | Reason::OffsetOutOfRange
        | Reason::BatchTooLarge
        | Reason::TooManyDigests
        | Reason::MessageTooLarge
        | Reason::UnsupportedDigestFunction
        | Reason::CredentialsMissing
        | Reason::CredentialsInvalid
        | Reason::ScopeMissing
        | Reason::MethodNotSupported
        | Reason::StorageUnavailable
        | Reason::StorageNotWritable
        | Reason::StorageFreeSpaceLow
        | Reason::ShuttingDown
        | Reason::Internal => aip_status(reason.code()),
    }
}

/// AIP-193's HTTP status for a canonical code (`google.rpc.Code`'s mapping).
#[must_use]
pub fn aip_status(code: Code) -> StatusCode {
    match code {
        Code::InvalidArgument | Code::FailedPrecondition | Code::OutOfRange => {
            StatusCode::BAD_REQUEST
        }
        Code::Unauthenticated => StatusCode::UNAUTHORIZED,
        Code::PermissionDenied => StatusCode::FORBIDDEN,
        Code::NotFound => StatusCode::NOT_FOUND,
        Code::Aborted | Code::AlreadyExists => StatusCode::CONFLICT,
        Code::ResourceExhausted => StatusCode::TOO_MANY_REQUESTS,
        // 499 Client Closed Request: not in the standard registry, but always valid.
        Code::Cancelled => StatusCode::from_u16(499).unwrap_or(StatusCode::BAD_REQUEST),
        Code::Unknown | Code::Internal | Code::DataLoss => StatusCode::INTERNAL_SERVER_ERROR,
        Code::Unimplemented => StatusCode::NOT_IMPLEMENTED,
        Code::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        Code::DeadlineExceeded => StatusCode::GATEWAY_TIMEOUT,
    }
}

/// An error response in the shape of AIP-193: a catalogue [`Status`] at its HTTP status.
#[derive(Debug, Clone)]
pub struct ApiError {
    status: Status,
    http: StatusCode,
    challenge: bool,
    /// The core error behind it, for the log line [`render`] writes.
    source: Option<Arc<cache::Error>>,
}

impl ApiError {
    /// An error for `reason`, at the status [`http_status`] gives it.
    pub fn new(reason: Reason, message: impl Into<std::borrow::Cow<'static, str>>) -> Self {
        Status::new(reason, message).into()
    }

    /// Adds an `ErrorInfo.metadata` entry.
    #[must_use]
    pub fn with(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.status = self.status.with(key, value);
        self
    }

    /// The same error at another HTTP status: for rewriting a response that already has one.
    #[must_use]
    fn at(mut self, http: StatusCode) -> Self {
        self.http = http;
        self
    }

    /// The HTTP status.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.http
    }

    /// The `ErrorInfo.reason`.
    #[must_use]
    pub fn reason(&self) -> Reason {
        self.status.reason()
    }

    /// `404 NOT_FOUND` for a missing cache entry. Not an error for any client: it builds.
    #[must_use]
    pub fn entry_not_found(key: &CacheKey) -> Self {
        let field = match key.namespace() {
            Namespace::Gradle => "key",
            Namespace::Maven => "path",
            Namespace::BazelAc | Namespace::BazelCas => "digest",
        };
        Self::new(Reason::EntryNotFound, "Cache entry not found.").with(field, key.name())
    }

    /// `401 UNAUTHENTICATED`, with the `WWW-Authenticate` challenge every `401` carries: Maven
    /// sends credentials on `GET` only after it.
    #[must_use]
    pub fn unauthenticated(
        reason: Reason,
        message: impl Into<std::borrow::Cow<'static, str>>,
    ) -> Self {
        let mut error = Self::new(reason, message);
        error.challenge = true;
        error
    }

    /// `403 PERMISSION_DENIED`: the user is known but lacks `scope`.
    #[must_use]
    pub fn permission_denied(user: &str, scope: Scope) -> Self {
        Self::new(
            Reason::ScopeMissing,
            format!("User {user:?} lacks the {scope} scope."),
        )
        .with("user", user)
        .with("requiredScope", scope.as_str())
    }

    /// `405` for a method the route does not support.
    #[must_use]
    pub fn method_not_allowed(method: &Method) -> Self {
        Self::new(
            Reason::MethodNotAllowed,
            format!("{method} is not supported on this route."),
        )
    }

    /// `404 NOT_FOUND` for an unknown route.
    #[must_use]
    pub fn route_not_found() -> Self {
        Self::new(Reason::RouteNotFound, "No such route.")
    }

    /// `400 INVALID_ARGUMENT` for a Bazel CAS upload whose SHA-256 is not its digest.
    #[must_use]
    pub fn digest_mismatch(key: &CacheKey) -> Self {
        Self::new(
            Reason::DigestMismatch,
            "The uploaded content does not hash to its digest.",
        )
        .with("digest", key.name())
    }

    /// `501 UNIMPLEMENTED` for `method`, which this server does not offer, such as `GET /metrics`
    /// with metrics off.
    #[must_use]
    pub fn method_not_supported(
        method: &str,
        message: impl Into<std::borrow::Cow<'static, str>>,
    ) -> Self {
        Self::new(Reason::MethodNotSupported, message).with("method", method)
    }
}

impl From<Status> for ApiError {
    fn from(status: Status) -> Self {
        Self {
            http: http_status(status.reason()),
            status,
            challenge: false,
            source: None,
        }
    }
}

impl From<InvalidKey> for ApiError {
    fn from(e: InvalidKey) -> Self {
        let reason = match e {
            InvalidKey::Gradle => Reason::InvalidCacheKey,
            InvalidKey::Maven => Reason::InvalidCachePath,
            InvalidKey::Bazel => Reason::InvalidDigest,
        };
        Self::new(reason, sentence(&e))
    }
}

impl From<cache::Error> for ApiError {
    fn from(e: cache::Error) -> Self {
        let mut error = Self::from(Status::from(&e));
        error.source = Some(Arc::new(e));
        error
    }
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
    metadata: &'a BTreeMap<&'a str, &'a str>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let metadata: BTreeMap<&str, &str> = self
            .status
            .metadata()
            .iter()
            .map(|(key, value)| (*key, value.as_str()))
            .collect();
        let body = Body {
            error: RpcStatus {
                code: self.http.as_u16(),
                message: self.status.message(),
                status: self.status.code().as_str(),
                details: [ErrorInfo {
                    type_url: ERROR_INFO_TYPE,
                    reason: self.status.reason().as_str(),
                    domain: DOMAIN,
                    metadata: &metadata,
                }],
            },
        };
        let mut response = (self.http, Json(body)).into_response();
        response.extensions_mut().insert(Rendered {
            reason: self.status.reason(),
            source: self.source,
        });
        if self.challenge {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, CHALLENGE);
        }
        response
    }
}

/// What [`render`] logs about an error response, left in its extensions by `into_response`.
#[derive(Debug, Clone)]
struct Rendered {
    reason: Reason,
    source: Option<Arc<cache::Error>>,
}

/// Middleware that logs each error response once, at a level chosen by its code: a server fault
/// at `error` with the source chain, a storage outage at `warn`, and everything else, the client's
/// doing or the node's state as `/readyz` reports it, at `debug`.
///
/// An error response that is not an [`ApiError`], such as axum's own `405` or a rejection from an
/// extractor, is rewritten as one at the same status, so every error a client sees is AIP-193.
pub(super) async fn render(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let rendered = match response.extensions_mut().remove::<Rendered>() {
        Some(rendered) => rendered,
        None if response.status().is_client_error() || response.status().is_server_error() => {
            response = rewrite(response).await;
            match response.extensions_mut().remove::<Rendered>() {
                Some(rendered) => rendered,
                None => return response,
            }
        }
        None => return response,
    };
    let Rendered { reason, source } = rendered;
    let status = response.status().as_u16();
    let code = reason.code();
    let reason = reason.as_str();
    match (code, source) {
        (Code::Internal | Code::Unknown | Code::DataLoss, Some(e)) => {
            tracing::error!(reason, status, error = %e, "request failed");
        }
        (Code::Internal | Code::Unknown | Code::DataLoss, None) => {
            tracing::error!(reason, status, "request failed");
        }
        (Code::Unavailable, Some(e)) => {
            tracing::warn!(reason, status, error = %e, "storage unavailable");
        }
        (_, Some(e)) => tracing::debug!(reason, status, error = %e, "request refused"),
        (_, None) => tracing::debug!(reason, status, "request refused"),
    }
    response
}

/// The longest rejection text kept as the message of a rewritten `4xx`.
const REWRITTEN_MESSAGE_BYTES: usize = 512;

/// An error response axum or a layer built without [`ApiError`], as AIP-193 at the same status.
/// Headers stay, `Allow` and `WWW-Authenticate` among them; the body is replaced.
async fn rewrite(response: Response) -> Response {
    let (parts, body) = response.into_parts();
    let status = parts.status;
    let error = match status {
        StatusCode::NOT_FOUND => ApiError::route_not_found(),
        StatusCode::METHOD_NOT_ALLOWED => ApiError::new(
            Reason::MethodNotAllowed,
            "The method is not supported on this route.",
        ),
        _ if status.is_client_error() => {
            // A rejection says what was wrong with the request; keep it as the message.
            let text = axum::body::to_bytes(body, REWRITTEN_MESSAGE_BYTES)
                .await
                .ok()
                .and_then(|bytes| String::from_utf8(bytes.to_vec()).ok())
                .filter(|text| !text.trim().is_empty());
            let message = text.map_or_else(
                || "The request is not valid.".to_owned(),
                |text| sentence(&text.trim()),
            );
            ApiError::new(Reason::InvalidRequest, message)
        }
        _ => ApiError::new(Reason::Internal, "Internal error."),
    };
    let mut rewritten = error.at(status).into_response();
    for (name, value) in &parts.headers {
        if name != header::CONTENT_TYPE && name != header::CONTENT_LENGTH {
            rewritten.headers_mut().insert(name, value.clone());
        }
    }
    rewritten
}

/// The `500 INTERNAL` for a handler that panicked, carrying the panic message to the log line.
// By value: the signature `CatchPanicLayer::custom` takes.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn panicked(panic: Box<dyn std::any::Any + Send + 'static>) -> Response {
    let message = panic
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a non-string payload".to_owned());
    ApiError::from(cache::Error::new(
        Code::Internal,
        anyhow::anyhow!("handler panicked: {message}"),
    ))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_reason_renders_as_aip_193() {
        for &reason in Reason::ALL {
            let response = ApiError::new(reason, "Message.")
                .with("k", "v")
                .into_response();
            assert_eq!(response.status(), http_status(reason), "{reason:?}");
            let body = axum::body::to_bytes(response.into_body(), 1 << 16).await;
            let json: serde_json::Value = serde_json::from_slice(&body.unwrap()).unwrap();
            let error = &json["error"];
            assert_eq!(error["code"], http_status(reason).as_u16(), "{reason:?}");
            assert_eq!(error["status"], reason.code().as_str(), "{reason:?}");
            assert_eq!(error["message"], "Message.");
            let info = &error["details"][0];
            assert_eq!(info["@type"], ERROR_INFO_TYPE);
            assert_eq!(info["reason"], reason.as_str());
            assert_eq!(info["domain"], DOMAIN);
            assert_eq!(info["metadata"]["k"], "v");
        }
    }

    /// The lines logged while a request runs through a router whose one route answers `error`.
    async fn logged(error: ApiError) -> Vec<String> {
        logged_response(error.into_response()).await
    }

    /// The lines logged while a request runs through a router whose one route answers `response`.
    async fn logged_response(response: Response) -> Vec<String> {
        use std::{
            io,
            sync::{Arc, Mutex},
        };

        use axum::{Router, middleware, routing::get};
        use tower::ServiceExt;

        #[derive(Clone, Default)]
        struct Lines(Arc<Mutex<Vec<u8>>>);
        impl io::Write for Lines {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let once = Arc::new(Mutex::new(Some(response)));
        let lines = Lines::default();
        let writer = lines.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let app = Router::new()
            .route(
                "/",
                get(move || async move { once.lock().unwrap().take().unwrap() }),
            )
            .layer(middleware::from_fn(render));
        let request = axum::http::Request::new(axum::body::Body::empty());
        app.oneshot(request).await.unwrap();
        let text = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
        text.lines().map(str::to_owned).collect()
    }

    #[tokio::test]
    async fn a_server_fault_is_logged_once_with_its_source() {
        let error = ApiError::from(cache::Error::from(std::io::Error::other("disk on fire")));
        let lines = logged(error).await;
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("ERROR"), "{lines:?}");
        assert!(lines[0].contains("reason=\"INTERNAL\""), "{lines:?}");
        assert!(lines[0].contains("disk on fire"), "{lines:?}");
    }

    #[tokio::test]
    async fn client_errors_and_node_state_log_at_debug() {
        for error in [
            ApiError::new(Reason::InvalidCacheKey, "Bad key."),
            ApiError::new(Reason::ShuttingDown, "Draining."),
            ApiError::from(cache::Error::from(cache::upload_error("reset"))),
        ] {
            let lines = logged(error).await;
            assert_eq!(lines.len(), 1, "{lines:?}");
            assert!(lines[0].contains("DEBUG"), "{lines:?}");
        }
    }

    /// The status, `Allow` header and AIP-193 JSON body of a request through the render and panic
    /// layers, over routes that answer without [`ApiError`].
    async fn rendered(
        method: Method,
        uri: &str,
    ) -> (StatusCode, Option<String>, serde_json::Value) {
        use axum::{Router, middleware, routing::get};
        use tower::ServiceExt;
        use tower_http::catch_panic::CatchPanicLayer;

        let app: Router = Router::new()
            .route("/ok", get(|| async { "ok" }))
            .route(
                "/plain",
                get(|| async { (StatusCode::BAD_REQUEST, "bad query") }),
            )
            .route(
                "/panic",
                get(|| async {
                    panic!("boom");
                    #[allow(unreachable_code)]
                    ""
                }),
            )
            .layer(CatchPanicLayer::custom(panicked))
            .layer(middleware::from_fn(render));
        let request = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let allow = response
            .headers()
            .get(header::ALLOW)
            .map(|v| v.to_str().unwrap().to_owned());
        let body = axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .unwrap();
        (
            status,
            allow,
            serde_json::from_slice(&body).unwrap_or_default(),
        )
    }

    #[tokio::test]
    async fn responses_built_without_api_error_are_rewritten() {
        let (status, allow, json) = rendered(Method::POST, "/ok").await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(allow.as_deref(), Some("GET,HEAD"));
        assert_eq!(json["error"]["details"][0]["reason"], "METHOD_NOT_ALLOWED");

        let (status, _, json) = rendered(Method::GET, "/nowhere").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json["error"]["details"][0]["reason"], "ROUTE_NOT_FOUND");

        let (status, _, json) = rendered(Method::GET, "/plain").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["error"]["code"], 400);
        assert_eq!(json["error"]["message"], "Bad query.");
        assert_eq!(json["error"]["details"][0]["reason"], "INVALID_REQUEST");

        let (status, _, json) = rendered(Method::GET, "/ok").await;
        assert_eq!(status, StatusCode::OK);
        assert!(json.is_null());
    }

    #[tokio::test]
    async fn a_panic_is_500_internal_and_logged_with_its_message() {
        let (status, _, json) = rendered(Method::GET, "/panic").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(json["error"]["status"], "INTERNAL");
        assert_eq!(json["error"]["message"], "Internal error.");
        assert_eq!(json["error"]["details"][0]["reason"], "INTERNAL");

        let response = panicked(Box::new("boom"));
        let lines = logged_response(response).await;
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("handler panicked: boom"), "{lines:?}");
    }

    #[test]
    fn every_401_carries_a_challenge() {
        let response = ApiError::unauthenticated(Reason::CredentialsMissing, "No credentials.")
            .into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers()[header::WWW_AUTHENTICATE],
            r#"Basic realm="mammoth-cache""#
        );
    }
}
