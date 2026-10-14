//! gRPC errors in the AIP-193 shape: a `google.rpc.Status` whose details carry one
//! `google.rpc.ErrorInfo` in the `mammoth-cache.dev` domain. Reasons and metadata come from the
//! catalogue in [`crate::api::error`], so a failure has the same reason here as over HTTP; the
//! canonical code becomes the `tonic::Code` one to one.

use std::borrow::Cow;

use prost::Message;
use tonic::Status;
use tonic_types::{ErrorDetails, StatusExt};

use super::proto::google::rpc;
use crate::{
    api::error::{self, Code, DOMAIN, Reason},
    cache,
    key::CacheKey,
};

/// The gRPC code for a canonical code.
#[must_use]
pub(crate) const fn code(code: Code) -> tonic::Code {
    match code {
        Code::Cancelled => tonic::Code::Cancelled,
        Code::Unknown => tonic::Code::Unknown,
        Code::InvalidArgument => tonic::Code::InvalidArgument,
        Code::DeadlineExceeded => tonic::Code::DeadlineExceeded,
        Code::NotFound => tonic::Code::NotFound,
        Code::AlreadyExists => tonic::Code::AlreadyExists,
        Code::PermissionDenied => tonic::Code::PermissionDenied,
        Code::ResourceExhausted => tonic::Code::ResourceExhausted,
        Code::FailedPrecondition => tonic::Code::FailedPrecondition,
        Code::Aborted => tonic::Code::Aborted,
        Code::OutOfRange => tonic::Code::OutOfRange,
        Code::Unimplemented => tonic::Code::Unimplemented,
        Code::Internal => tonic::Code::Internal,
        Code::Unavailable => tonic::Code::Unavailable,
        Code::DataLoss => tonic::Code::DataLoss,
        Code::Unauthenticated => tonic::Code::Unauthenticated,
    }
}

impl From<&error::Status> for Status {
    fn from(status: &error::Status) -> Self {
        let metadata = status
            .metadata()
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect::<std::collections::HashMap<_, _>>();
        Self::with_error_details(
            code(status.code()),
            status.message(),
            ErrorDetails::with_error_info(status.reason().as_str(), DOMAIN, metadata),
        )
    }
}

impl From<error::Status> for Status {
    fn from(status: error::Status) -> Self {
        Self::from(&status)
    }
}

/// A status for `reason`, without metadata.
pub(crate) fn error(reason: Reason, message: impl Into<Cow<'static, str>>) -> Status {
    error::Status::new(reason, message).into()
}

/// The `google.rpc.Status` for one blob of a batch call: the same code, message and details the
/// call would fail with.
pub(crate) fn to_rpc(status: &Status) -> rpc::Status {
    if !status.details().is_empty()
        && let Ok(decoded) = rpc::Status::decode(status.details())
    {
        return decoded;
    }
    rpc::Status {
        code: status.code() as i32,
        message: status.message().to_owned(),
        details: Vec::new(),
    }
}

/// `OK` for one blob of a batch call.
pub(crate) fn ok() -> rpc::Status {
    rpc::Status::default()
}

/// A miss. Bazel names an entry by its digest, on HTTP and gRPC alike, so the metadata says
/// `digest`, as it does for the mismatches.
pub(crate) fn not_found(key: &CacheKey) -> Status {
    error::Status::new(Reason::EntryNotFound, format!("No entry {}.", key.name()))
        .with("digest", key.name())
        .into()
}

pub(crate) fn digest_mismatch(key: &CacheKey) -> Status {
    error::Status::new(
        Reason::DigestMismatch,
        "The content does not hash to its digest.",
    )
    .with("digest", key.name())
    .into()
}

/// A storage error as a status, logged here because only here is its source chain. A bucket
/// outage never gets here: the core turns it into misses.
pub(crate) fn storage(e: &cache::Error) -> Status {
    let status = error::Status::from(e);
    match status.code() {
        Code::Unavailable => crate::warn_throttled!(error = %e, "storage unavailable"),
        Code::Internal => crate::error_throttled!(error = %e, "storage error"),
        // The client's fault, and usually the client is gone.
        _ => tracing::debug!(error = %e, "upload failed"),
    }
    status.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn details_carry_error_info() {
        let status: Status = error::Status::new(Reason::ScopeMissing, "no")
            .with("user", "ci")
            .into();
        assert_eq!(status.code(), tonic::Code::PermissionDenied);
        assert_eq!(status.message(), "no");
        let info = status.get_details_error_info().unwrap();
        assert_eq!(info.reason, "SCOPE_MISSING");
        assert_eq!(info.domain, DOMAIN);
        assert_eq!(info.metadata["user"], "ci");

        let rpc = to_rpc(&status);
        assert_eq!(rpc.code, tonic::Code::PermissionDenied as i32);
        assert_eq!(rpc.message, "no");
        assert_eq!(
            rpc.details[0].type_url,
            "type.googleapis.com/google.rpc.ErrorInfo"
        );
    }

    #[test]
    fn every_reason_keeps_its_code() {
        for &reason in Reason::ALL {
            let status = error(reason, "x");
            assert_eq!(status.code(), code(reason.code()), "{reason:?}");
            let info = status.get_details_error_info().unwrap();
            assert_eq!(info.reason, reason.as_str());
        }
    }

    #[test]
    fn too_large_is_not_retryable() {
        let status = storage(&cache::Error::too_large(Some(10), 5));
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("10 bytes"));
        let info = status.get_details_error_info().unwrap();
        assert_eq!(info.reason, "ENTRY_TOO_LARGE");
        assert_eq!(info.metadata["maxEntryBytes"], "5");
        assert_eq!(info.metadata["entryBytes"], "10");
    }

    /// Parity between the adapters (design doc 0017): an error renders to the same reason,
    /// message, canonical code and metadata over HTTP as over gRPC.
    mod parity {
        use std::{collections::HashMap, io};

        use axum::response::IntoResponse;
        use proptest::prelude::*;

        use super::*;
        use crate::{
            http::error::ApiError,
            key::{InvalidKey, Namespace},
            reapi::{digest::Blob, proto::build::bazel::remote::execution::v2::Digest},
        };

        /// `google.rpc.Code` names by number, so the gRPC code is checked against the HTTP body's
        /// `status` without going through [`code`].
        const CANONICAL: [&str; 17] = [
            "OK",
            "CANCELLED",
            "UNKNOWN",
            "INVALID_ARGUMENT",
            "DEADLINE_EXCEEDED",
            "NOT_FOUND",
            "ALREADY_EXISTS",
            "PERMISSION_DENIED",
            "RESOURCE_EXHAUSTED",
            "FAILED_PRECONDITION",
            "ABORTED",
            "OUT_OF_RANGE",
            "UNIMPLEMENTED",
            "INTERNAL",
            "UNAVAILABLE",
            "DATA_LOSS",
            "UNAUTHENTICATED",
        ];

        const CODES: [Code; 16] = [
            Code::Cancelled,
            Code::Unknown,
            Code::InvalidArgument,
            Code::DeadlineExceeded,
            Code::NotFound,
            Code::AlreadyExists,
            Code::PermissionDenied,
            Code::ResourceExhausted,
            Code::FailedPrecondition,
            Code::Aborted,
            Code::OutOfRange,
            Code::Unimplemented,
            Code::Internal,
            Code::Unavailable,
            Code::DataLoss,
            Code::Unauthenticated,
        ];

        const KINDS: [io::ErrorKind; 6] = [
            io::ErrorKind::TimedOut,
            io::ErrorKind::WouldBlock,
            io::ErrorKind::NotFound,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::StorageFull,
            io::ErrorKind::Other,
        ];

        /// Any error the core can hand an adapter.
        fn core_error() -> impl Strategy<Value = cache::Error> {
            prop_oneof![
                (proptest::option::of(any::<u64>()), any::<u64>())
                    .prop_map(|(size, limit)| cache::Error::too_large(size, limit)),
                (0..CODES.len(), ".{0,40}")
                    .prop_map(|(i, what)| cache::Error::new(CODES[i], anyhow::anyhow!(what))),
                (0..KINDS.len(), ".{0,40}")
                    .prop_map(|(i, what)| cache::Error::from(io::Error::new(KINDS[i], what))),
                ".{0,40}".prop_map(|what| cache::Error::from(cache::upload_error(what))),
            ]
        }

        /// What a client reads from an error, the same fields on both transports.
        #[derive(Debug, PartialEq, Eq)]
        struct Seen {
            code: String,
            message: String,
            reason: String,
            domain: String,
            metadata: HashMap<String, String>,
        }

        fn over_http(error: ApiError) -> Seen {
            let response = error.into_response();
            let body = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(axum::body::to_bytes(response.into_body(), 1 << 16))
                .unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let error = &json["error"];
            let info = &error["details"][0];
            let metadata = info["metadata"].as_object().map_or_else(HashMap::new, |m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                    .collect()
            });
            Seen {
                code: error["status"].as_str().unwrap().to_owned(),
                message: error["message"].as_str().unwrap().to_owned(),
                reason: info["reason"].as_str().unwrap().to_owned(),
                domain: info["domain"].as_str().unwrap().to_owned(),
                metadata,
            }
        }

        fn over_grpc(status: &Status) -> Seen {
            let info = status.get_details_error_info().unwrap();
            Seen {
                code: CANONICAL[status.code() as usize].to_owned(),
                message: status.message().to_owned(),
                reason: info.reason,
                domain: info.domain,
                metadata: info.metadata,
            }
        }

        proptest! {
            #[test]
            fn core_errors_render_alike(error in core_error()) {
                let grpc = over_grpc(&storage(&error));
                let http = over_http(ApiError::from(error));
                prop_assert_eq!(http, grpc);
            }

            /// A Bazel digest the node refuses, in a URL path or a `Digest` message.
            #[test]
            fn invalid_digests_render_alike(
                hash in "[0-9a-fA-Fg-z]{0,70}",
                ac in any::<bool>(),
            ) {
                let namespace = if ac { Namespace::BazelAc } else { Namespace::BazelCas };
                let Err(invalid) = CacheKey::bazel(namespace, &hash) else {
                    return Err(TestCaseError::reject("a valid digest"));
                };
                prop_assert_eq!(invalid, InvalidKey::Bazel);
                let digest = Digest { hash, size_bytes: 1 };
                let parsed = if ac { Blob::action(Some(&digest)) } else { Blob::cas(Some(&digest)) };
                let Err(refused) = parsed else {
                    return Err(TestCaseError::fail("gRPC accepted a digest HTTP refuses"));
                };
                let grpc = over_grpc(&refused);
                let http = over_http(ApiError::from(invalid));
                // The reason, code and metadata are the contract; each transport words its own
                // message for the place the digest came from.
                prop_assert_eq!(
                    (&http.code, &http.reason, &http.domain, &http.metadata),
                    (&grpc.code, &grpc.reason, &grpc.domain, &grpc.metadata)
                );
            }
        }
    }
}
