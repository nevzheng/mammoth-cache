//! The error catalogue: every AIP-193 reason this server sends, and the core-to-reason map the
//! adapters share (design doc 0017).
//!
//! The core describes failures with canonical codes ([`cache::Code`]); this module turns them into
//! a [`Status`]: a stable [`Reason`], a message and metadata, still without any transport. Each
//! adapter renders a `Status` for its protocol, overriding the status code where the protocol has
//! its own rules.

use std::borrow::Cow;

pub use crate::cache::Code;
use crate::cache::{self, Detail};

/// The `ErrorInfo.domain` of every error this server returns.
pub const DOMAIN: &str = "mammoth-cache.dev";

/// A stable AIP-193 `ErrorInfo.reason`. Never renamed or reused: clients and dashboards match on
/// them. `docs/design/0017-errors.md` and `api/openapi/common.yaml` list every one, and a test
/// keeps all three equal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reason {
    /// No entry under the key. Metadata: `key`, `path` or `digest`, the name as requested.
    EntryNotFound,
    /// No route for the path.
    RouteNotFound,
    /// A request the router or an extractor refused, rendered at the status it was sent with.
    InvalidRequest,
    /// The entry is over the size limit. Metadata: `maxEntryBytes` and, when declared,
    /// `entryBytes`.
    EntryTooLarge,
    /// A CAS upload whose content does not hash to its digest. Metadata: `digest`.
    DigestMismatch,
    /// An upload whose length is not its digest's size. Metadata: `digest`.
    SizeMismatch,
    /// The upload ended before it was complete.
    UploadIncomplete,
    /// The client went away mid-upload.
    UploadCancelled,
    /// The client sent nothing for too long mid-upload.
    UploadIdle,
    /// The client read nothing for too long mid-download.
    DownloadStalled,
    /// A `ByteStream.Write` with no data and no `finish_write`.
    EmptyWrite,
    /// A compressor this server does not offer.
    UnsupportedCompressor,
    /// An `ActionResult` that is missing from an `UpdateActionResult` call.
    ActionResultMissing,
    /// A Gradle cache key that is not one.
    InvalidCacheKey,
    /// A Maven path that is not one.
    InvalidCachePath,
    /// A Bazel or REAPI digest that is not one.
    InvalidDigest,
    /// A `ByteStream` resource name that is not one.
    InvalidResourceName,
    /// A negative `read_limit`.
    InvalidReadLimit,
    /// A `ByteStream.Write` whose messages do not follow the protocol.
    InvalidWrite,
    /// A `read_offset` past the end of the blob.
    OffsetOutOfRange,
    /// A batch over the size limit.
    BatchTooLarge,
    /// A `FindMissingBlobs` call with too many digests.
    TooManyDigests,
    /// A gRPC message over the decoding limit.
    MessageTooLarge,
    /// A digest function this server does not offer.
    UnsupportedDigestFunction,
    /// No credentials, where they are required.
    CredentialsMissing,
    /// Credentials that match no user.
    CredentialsInvalid,
    /// The user lacks a scope. Metadata: `user`, `requiredScope`.
    ScopeMissing,
    /// An HTTP method the route does not support.
    MethodNotAllowed,
    /// A method this server does not offer. Metadata: `method`.
    MethodNotSupported,
    /// A tier failed in a way that may pass.
    StorageUnavailable,
    /// A local tier cannot write. Metadata: `check`, tier details.
    StorageNotWritable,
    /// A local tier is short of free space. Metadata: `check`, tier details.
    StorageFreeSpaceLow,
    /// The node is draining. Metadata: `check`.
    ShuttingDown,
    /// Anything else: a bug or an unexpected storage failure.
    Internal,
}

impl Reason {
    /// Every reason, in catalogue order.
    pub const ALL: &[Self] = &[
        Self::EntryNotFound,
        Self::RouteNotFound,
        Self::InvalidRequest,
        Self::EntryTooLarge,
        Self::DigestMismatch,
        Self::SizeMismatch,
        Self::UploadIncomplete,
        Self::UploadCancelled,
        Self::UploadIdle,
        Self::DownloadStalled,
        Self::EmptyWrite,
        Self::UnsupportedCompressor,
        Self::ActionResultMissing,
        Self::InvalidCacheKey,
        Self::InvalidCachePath,
        Self::InvalidDigest,
        Self::InvalidResourceName,
        Self::InvalidReadLimit,
        Self::InvalidWrite,
        Self::OffsetOutOfRange,
        Self::BatchTooLarge,
        Self::TooManyDigests,
        Self::MessageTooLarge,
        Self::UnsupportedDigestFunction,
        Self::CredentialsMissing,
        Self::CredentialsInvalid,
        Self::ScopeMissing,
        Self::MethodNotAllowed,
        Self::MethodNotSupported,
        Self::StorageUnavailable,
        Self::StorageNotWritable,
        Self::StorageFreeSpaceLow,
        Self::ShuttingDown,
        Self::Internal,
    ];

    /// The `ErrorInfo.reason` string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EntryNotFound => "ENTRY_NOT_FOUND",
            Self::RouteNotFound => "ROUTE_NOT_FOUND",
            Self::InvalidRequest => "INVALID_REQUEST",
            Self::EntryTooLarge => "ENTRY_TOO_LARGE",
            Self::DigestMismatch => "DIGEST_MISMATCH",
            Self::SizeMismatch => "SIZE_MISMATCH",
            Self::UploadIncomplete => "UPLOAD_INCOMPLETE",
            Self::UploadCancelled => "UPLOAD_CANCELLED",
            Self::UploadIdle => "UPLOAD_IDLE",
            Self::DownloadStalled => "DOWNLOAD_STALLED",
            Self::EmptyWrite => "EMPTY_WRITE",
            Self::UnsupportedCompressor => "UNSUPPORTED_COMPRESSOR",
            Self::ActionResultMissing => "ACTION_RESULT_MISSING",
            Self::InvalidCacheKey => "INVALID_CACHE_KEY",
            Self::InvalidCachePath => "INVALID_CACHE_PATH",
            Self::InvalidDigest => "INVALID_DIGEST",
            Self::InvalidResourceName => "INVALID_RESOURCE_NAME",
            Self::InvalidReadLimit => "INVALID_READ_LIMIT",
            Self::InvalidWrite => "INVALID_WRITE",
            Self::OffsetOutOfRange => "OFFSET_OUT_OF_RANGE",
            Self::BatchTooLarge => "BATCH_TOO_LARGE",
            Self::TooManyDigests => "TOO_MANY_DIGESTS",
            Self::MessageTooLarge => "MESSAGE_TOO_LARGE",
            Self::UnsupportedDigestFunction => "UNSUPPORTED_DIGEST_FUNCTION",
            Self::CredentialsMissing => "CREDENTIALS_MISSING",
            Self::CredentialsInvalid => "CREDENTIALS_INVALID",
            Self::ScopeMissing => "SCOPE_MISSING",
            Self::MethodNotAllowed => "METHOD_NOT_ALLOWED",
            Self::MethodNotSupported => "METHOD_NOT_SUPPORTED",
            Self::StorageUnavailable => "STORAGE_UNAVAILABLE",
            Self::StorageNotWritable => "STORAGE_NOT_WRITABLE",
            Self::StorageFreeSpaceLow => "STORAGE_FREE_SPACE_LOW",
            Self::ShuttingDown => "SHUTTING_DOWN",
            Self::Internal => "INTERNAL",
        }
    }

    /// The canonical code the reason is sent with, before any protocol's override.
    #[must_use]
    pub const fn code(self) -> Code {
        match self {
            Self::EntryNotFound | Self::RouteNotFound => Code::NotFound,
            Self::InvalidRequest
            | Self::EntryTooLarge
            | Self::DigestMismatch
            | Self::SizeMismatch
            | Self::UploadIncomplete
            | Self::EmptyWrite
            | Self::UnsupportedCompressor
            | Self::ActionResultMissing
            | Self::InvalidCacheKey
            | Self::InvalidCachePath
            | Self::InvalidDigest
            | Self::InvalidResourceName
            | Self::InvalidReadLimit
            | Self::InvalidWrite
            | Self::BatchTooLarge
            | Self::TooManyDigests
            | Self::UnsupportedDigestFunction => Code::InvalidArgument,
            Self::UploadCancelled => Code::Cancelled,
            Self::UploadIdle | Self::DownloadStalled => Code::DeadlineExceeded,
            Self::OffsetOutOfRange => Code::OutOfRange,
            Self::MessageTooLarge => Code::ResourceExhausted,
            Self::CredentialsMissing | Self::CredentialsInvalid => Code::Unauthenticated,
            Self::ScopeMissing => Code::PermissionDenied,
            Self::MethodNotAllowed | Self::MethodNotSupported => Code::Unimplemented,
            Self::StorageUnavailable
            | Self::StorageNotWritable
            | Self::StorageFreeSpaceLow
            | Self::ShuttingDown => Code::Unavailable,
            Self::Internal => Code::Internal,
        }
    }
}

/// An AIP-193 error without the transport: the reason, a message for people, and the
/// `ErrorInfo.metadata`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    reason: Reason,
    message: Cow<'static, str>,
    metadata: Vec<(&'static str, String)>,
}

impl Status {
    /// A status for `reason`.
    pub fn new(reason: Reason, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            reason,
            message: message.into(),
            metadata: Vec::new(),
        }
    }

    /// Adds an `ErrorInfo.metadata` entry, replacing one with the same key.
    #[must_use]
    pub fn with(mut self, key: &'static str, value: impl Into<String>) -> Self {
        let value = value.into();
        match self.metadata.iter_mut().find(|(k, _)| *k == key) {
            Some(entry) => entry.1 = value,
            None => self.metadata.push((key, value)),
        }
        self
    }

    #[must_use]
    pub fn reason(&self) -> Reason {
        self.reason
    }

    /// The canonical code, from the reason.
    #[must_use]
    pub fn code(&self) -> Code {
        self.reason.code()
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    #[must_use]
    pub fn metadata(&self) -> &[(&'static str, String)] {
        &self.metadata
    }
}

/// The core's errors as reasons, the same for every adapter. A miss is not an error, so
/// `ENTRY_NOT_FOUND` is built by the adapters.
impl From<&cache::Error> for Status {
    fn from(e: &cache::Error) -> Self {
        if let Some(&Detail::TooLarge { size, limit }) = e.detail() {
            let status = Self::new(Reason::EntryTooLarge, sentence(e))
                .with("maxEntryBytes", limit.to_string());
            return match size {
                Some(size) => status.with("entryBytes", size.to_string()),
                None => status,
            };
        }
        match e.code() {
            // Without a detail, the core sends INVALID_ARGUMENT only for an upload that failed.
            Code::InvalidArgument => Self::new(
                Reason::UploadIncomplete,
                "The request body ended before it was complete.",
            ),
            Code::Cancelled => {
                Self::new(Reason::UploadCancelled, "The client cancelled the upload.")
            }
            Code::DeadlineExceeded => Self::new(
                Reason::UploadIdle,
                "The client sent nothing for too long during the upload.",
            ),
            Code::Unavailable => Self::new(Reason::StorageUnavailable, "Storage is unavailable."),
            // DATA_LOSS never reaches a client: a read that finds it answers as a miss.
            Code::Unknown
            | Code::NotFound
            | Code::AlreadyExists
            | Code::PermissionDenied
            | Code::ResourceExhausted
            | Code::FailedPrecondition
            | Code::Aborted
            | Code::OutOfRange
            | Code::Unimplemented
            | Code::Internal
            | Code::DataLoss
            | Code::Unauthenticated => Self::new(Reason::Internal, "Internal error."),
        }
    }
}

/// Renders a Rust error message (lowercase, no full stop) as an API message.
pub(crate) fn sentence(e: &impl std::fmt::Display) -> String {
    let text = e.to_string();
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        format!("{}{}.", first.to_uppercase(), chars.as_str())
    })
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, io};

    use super::*;
    use crate::cache::upload_error;

    #[test]
    fn reasons_are_unique_and_well_formed() {
        let names: HashSet<_> = Reason::ALL.iter().map(|r| r.as_str()).collect();
        assert_eq!(names.len(), Reason::ALL.len());
        for name in names {
            assert!(name.len() <= 63, "{name}");
            assert!(
                name.bytes().all(|b| b.is_ascii_uppercase() || b == b'_')
                    && !name.starts_with('_')
                    && !name.ends_with('_'),
                "{name}"
            );
        }
    }

    #[test]
    fn core_errors_map_to_reasons() {
        let too_large = Status::from(&cache::Error::too_large(Some(5), 4));
        assert_eq!(too_large.reason(), Reason::EntryTooLarge);
        assert_eq!(too_large.code(), Code::InvalidArgument);
        assert_eq!(
            too_large.message(),
            "Cache entry is 5 bytes; the limit is 4."
        );
        assert_eq!(
            too_large.metadata(),
            [
                ("maxEntryBytes", "4".to_owned()),
                ("entryBytes", "5".to_owned())
            ]
        );
        let unknown_size = Status::from(&cache::Error::too_large(None, 4));
        assert_eq!(unknown_size.metadata(), [("maxEntryBytes", "4".to_owned())]);

        let cases = [
            (
                cache::Error::from(upload_error("reset")),
                Reason::UploadIncomplete,
            ),
            (
                cache::Error::new(Code::Cancelled, anyhow::anyhow!("gone")),
                Reason::UploadCancelled,
            ),
            (
                cache::Error::new(Code::DeadlineExceeded, anyhow::anyhow!("idle")),
                Reason::UploadIdle,
            ),
            (
                cache::Error::from(io::Error::new(io::ErrorKind::TimedOut, "slow")),
                Reason::StorageUnavailable,
            ),
            (
                cache::Error::from(io::Error::other("disk")),
                Reason::Internal,
            ),
            (
                cache::Error::new(Code::DataLoss, anyhow::anyhow!("corrupt")),
                Reason::Internal,
            ),
        ];
        for (error, reason) in cases {
            let status = Status::from(&error);
            assert_eq!(status.reason(), reason, "{error}");
            assert!(status.metadata().is_empty());
        }
    }

    #[test]
    fn metadata_keys_are_replaced_not_repeated() {
        let status = Status::new(Reason::Internal, "x")
            .with("a", "1")
            .with("a", "2");
        assert_eq!(status.metadata(), [("a", "2".to_owned())]);
    }
}
