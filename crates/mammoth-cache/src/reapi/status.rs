//! gRPC errors in the AIP-193 shape: a `google.rpc.Status` whose details carry one
//! `google.rpc.ErrorInfo` in the `mammoth-cache.dev` domain, with the same reasons as the HTTP
//! adapters where they overlap.

use prost::Message;
use tonic::{Code, Status};

use super::proto::google::rpc;
use crate::{cache::CacheError, key::CacheKey};

const DOMAIN: &str = "mammoth-cache.dev";

/// A status with an `ErrorInfo` detail.
pub(crate) fn error(
    code: Code,
    reason: &'static str,
    message: impl Into<String>,
    metadata: &[(&str, &str)],
) -> Status {
    let message = message.into();
    let info = rpc::ErrorInfo {
        reason: reason.to_owned(),
        domain: DOMAIN.to_owned(),
        metadata: metadata
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
    };
    let details = rpc::Status {
        code: code as i32,
        message: message.clone(),
        details: vec![prost_types::Any {
            type_url: "type.googleapis.com/google.rpc.ErrorInfo".to_owned(),
            value: info.encode_to_vec(),
        }],
    };
    Status::with_details(code, message, details.encode_to_vec().into())
}

/// The `google.rpc.Status` for one blob of a batch call.
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

pub(crate) fn invalid_argument(reason: &'static str, message: impl Into<String>) -> Status {
    error(Code::InvalidArgument, reason, message, &[])
}

pub(crate) fn not_found(key: &CacheKey) -> Status {
    error(
        Code::NotFound,
        "ENTRY_NOT_FOUND",
        format!("No entry {key}."),
        &[("key", key.as_str())],
    )
}

pub(crate) fn digest_mismatch(key: &CacheKey) -> Status {
    error(
        Code::InvalidArgument,
        "DIGEST_MISMATCH",
        "The content does not hash to its digest.",
        &[("key", key.as_str())],
    )
}

/// An entry over `limits.max_entry_size`. `INVALID_ARGUMENT`, not `RESOURCE_EXHAUSTED`: clients
/// retry the latter, and an entry that is too large stays too large.
pub(crate) fn too_large(size: Option<u64>, limit: u64) -> Status {
    let limit_text = limit.to_string();
    let message = match size {
        Some(size) => format!("The entry is {size} bytes; the limit is {limit}."),
        None => format!("The entry is over the limit of {limit} bytes."),
    };
    error(
        Code::InvalidArgument,
        "ENTRY_TOO_LARGE",
        message,
        &[("limit", &limit_text)],
    )
}

/// A storage error as a status. A bucket outage never gets here: the core turns it into misses.
pub(crate) fn storage(error: &CacheError) -> Status {
    match error {
        CacheError::TooLarge { size, limit } => too_large(*size, *limit),
        CacheError::Upload(e) => {
            tracing::debug!(error = %e, "upload failed");
            self::error(
                Code::InvalidArgument,
                "UPLOAD_INCOMPLETE",
                "The upload failed before it completed.",
                &[],
            )
        }
        CacheError::Unavailable(e) => {
            tracing::warn!(error = %e, "storage unavailable");
            self::error(
                Code::Unavailable,
                "STORAGE_UNAVAILABLE",
                "Storage is unavailable.",
                &[],
            )
        }
        CacheError::Internal(e) => {
            tracing::error!(error = %e, "storage error");
            self::error(Code::Internal, "STORAGE_ERROR", "Storage failed.", &[])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn details_carry_error_info() {
        let status = invalid_argument("INVALID_DIGEST", "bad");
        assert_eq!(status.code(), Code::InvalidArgument);
        let rpc = to_rpc(&status);
        assert_eq!(rpc.code, Code::InvalidArgument as i32);
        let info = rpc::ErrorInfo::decode(rpc.details[0].value.as_slice()).unwrap();
        assert_eq!(info.reason, "INVALID_DIGEST");
        assert_eq!(info.domain, DOMAIN);
    }

    #[test]
    fn too_large_is_not_retryable() {
        let status = too_large(Some(10), 5);
        assert_eq!(status.code(), Code::InvalidArgument);
        assert!(status.message().contains("10 bytes"));
    }
}
