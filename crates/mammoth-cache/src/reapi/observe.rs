//! Metrics for every gRPC call, taken outside authentication so refused calls count too:
//! `mammoth_grpc_requests_total` by method and status code, `mammoth_grpc_request_duration_seconds`
//! and `mammoth_grpc_requests_in_flight`.
//!
//! The status code is in the response headers when the call fails before answering
//! ("trailers-only"), and otherwise in the trailers at the end of the body, so the call is counted
//! when its body ends. A call whose response is dropped before then, because the client cancelled
//! or went away, counts as `CANCELLED`.

use std::{
    pin::Pin,
    task::{Context, Poll},
    time::Instant,
};

use axum::{
    body::{Body, Bytes},
    extract::Request,
    http::HeaderMap,
    middleware::Next,
    response::Response,
};
use hyper::body::{Frame, SizeHint};
use tonic::Code;

/// The methods counted by name; any other path is counted as `other`.
const METHODS: &[(&str, &str)] = &[
    (
        "build.bazel.remote.execution.v2.Capabilities",
        "GetCapabilities",
    ),
    (
        "build.bazel.remote.execution.v2.ActionCache",
        "GetActionResult",
    ),
    (
        "build.bazel.remote.execution.v2.ActionCache",
        "UpdateActionResult",
    ),
    (
        "build.bazel.remote.execution.v2.ContentAddressableStorage",
        "FindMissingBlobs",
    ),
    (
        "build.bazel.remote.execution.v2.ContentAddressableStorage",
        "BatchUpdateBlobs",
    ),
    (
        "build.bazel.remote.execution.v2.ContentAddressableStorage",
        "BatchReadBlobs",
    ),
    (
        "build.bazel.remote.execution.v2.ContentAddressableStorage",
        "GetTree",
    ),
    (
        "build.bazel.remote.execution.v2.ContentAddressableStorage",
        "SplitBlob",
    ),
    (
        "build.bazel.remote.execution.v2.ContentAddressableStorage",
        "SpliceBlob",
    ),
    ("google.bytestream.ByteStream", "Read"),
    ("google.bytestream.ByteStream", "Write"),
    ("google.bytestream.ByteStream", "QueryWriteStatus"),
];

/// The `method` label for a request path: the method's name, or `other`, so unknown paths add no
/// series.
fn method(path: &str) -> &'static str {
    let Some((service, name)) = path.strip_prefix('/').and_then(|p| p.split_once('/')) else {
        return "other";
    };
    METHODS
        .iter()
        .find(|(s, m)| *s == service && *m == name)
        .map_or("other", |(_, m)| m)
}

/// Middleware that counts and times each call.
pub(crate) async fn observe(request: Request, next: Next) -> Response {
    let mut call = Call::start(method(request.uri().path()));
    let response = next.run(request).await;
    // A trailers-only response: the call failed before answering.
    if let Some(code) = code(response.headers()) {
        call.code = Some(code);
    }
    response.map(|body| Body::new(Observed { body, call }))
}

/// One call, recorded when dropped.
struct Call {
    method: &'static str,
    started: Instant,
    code: Option<Code>,
}

impl Call {
    fn start(method: &'static str) -> Self {
        metrics::gauge!("mammoth_grpc_requests_in_flight", "method" => method).increment(1.0);
        Self {
            method,
            started: Instant::now(),
            code: None,
        }
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        let method = self.method;
        metrics::gauge!("mammoth_grpc_requests_in_flight", "method" => method).decrement(1.0);
        // No status by now: the client cancelled or went away first.
        let code = self.code.unwrap_or(Code::Cancelled);
        metrics::counter!(
            "mammoth_grpc_requests_total",
            "method" => method,
            "code" => code_label(code),
        )
        .increment(1);
        metrics::histogram!("mammoth_grpc_request_duration_seconds", "method" => method)
            .record(self.started.elapsed().as_secs_f64());
    }
}

/// A response body that finishes its call when it ends, reading the status from the trailers.
struct Observed {
    body: Body,
    call: Call,
}

impl hyper::body::Body for Observed {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let poll = Pin::new(&mut self.body).poll_frame(cx);
        match &poll {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(code) = frame.trailers_ref().and_then(code) {
                    self.call.code = Some(code);
                }
            }
            // The body ended, or broke, without a status: tonic always sends one in the
            // trailers, so the connection failed.
            Poll::Ready(Some(Err(_)) | None) => {
                self.call.code.get_or_insert(Code::Unknown);
            }
            Poll::Pending => {}
        }
        poll
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

/// The `grpc-status` in `headers`, if any.
fn code(headers: &HeaderMap) -> Option<Code> {
    let value = headers.get("grpc-status")?.to_str().ok()?;
    Some(Code::from_i32(value.parse().ok()?))
}

/// The status code's canonical name, as the `code` label.
pub(crate) fn code_label(code: Code) -> &'static str {
    match code {
        Code::Ok => "OK",
        Code::Cancelled => "CANCELLED",
        Code::Unknown => "UNKNOWN",
        Code::InvalidArgument => "INVALID_ARGUMENT",
        Code::DeadlineExceeded => "DEADLINE_EXCEEDED",
        Code::NotFound => "NOT_FOUND",
        Code::AlreadyExists => "ALREADY_EXISTS",
        Code::PermissionDenied => "PERMISSION_DENIED",
        Code::ResourceExhausted => "RESOURCE_EXHAUSTED",
        Code::FailedPrecondition => "FAILED_PRECONDITION",
        Code::Aborted => "ABORTED",
        Code::OutOfRange => "OUT_OF_RANGE",
        Code::Unimplemented => "UNIMPLEMENTED",
        Code::Internal => "INTERNAL",
        Code::Unavailable => "UNAVAILABLE",
        Code::DataLoss => "DATA_LOSS",
        Code::Unauthenticated => "UNAUTHENTICATED",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_known_methods_and_nothing_else() {
        assert_eq!(
            method("/build.bazel.remote.execution.v2.ContentAddressableStorage/FindMissingBlobs"),
            "FindMissingBlobs"
        );
        assert_eq!(method("/google.bytestream.ByteStream/Read"), "Read");
        assert_eq!(
            method("/build.bazel.remote.execution.v2.Execution/Execute"),
            "other"
        );
        assert_eq!(method("/google.bytestream.ByteStream/Nope"), "other");
        assert_eq!(method("/nonsense"), "other");
    }

    #[test]
    fn reads_the_status_header() {
        let mut headers = HeaderMap::new();
        assert_eq!(code(&headers), None);
        headers.insert("grpc-status", "16".parse().unwrap());
        assert_eq!(code(&headers), Some(Code::Unauthenticated));
    }
}
