//! The REAPI cache services over the cache core: `Capabilities`, `ActionCache`,
//! `ContentAddressableStorage` and `ByteStream` (design doc 0016). `Execution` is not served.

use std::{collections::HashSet, future::Future, io, pin::Pin, sync::Arc, time::Duration};

use bytes::{Bytes, BytesMut};
use futures_util::{Stream, StreamExt, TryStreamExt, stream};
use prost::Message;
use sha2::{Digest as _, Sha256};
use tonic::{Code, Request, Response, Status, Streaming};

use super::{
    digest::{self, Blob},
    proto::{
        build::bazel::{
            remote::execution::v2::{
                ActionCacheUpdateCapabilities, ActionResult, BatchReadBlobsRequest,
                BatchReadBlobsResponse, BatchUpdateBlobsRequest, BatchUpdateBlobsResponse,
                CacheCapabilities, Digest, ExecutionCapabilities, FindMissingBlobsRequest,
                FindMissingBlobsResponse, GetActionResultRequest, GetCapabilitiesRequest,
                GetTreeRequest, GetTreeResponse, ServerCapabilities, SpliceBlobRequest,
                SpliceBlobResponse, SplitBlobRequest, SplitBlobResponse, UpdateActionResultRequest,
                action_cache_server::ActionCache, batch_read_blobs_response,
                batch_update_blobs_response, capabilities_server::Capabilities,
                content_addressable_storage_server::ContentAddressableStorage, digest_function,
                symlink_absolute_path_strategy,
            },
            semver::SemVer,
        },
        google::bytestream::{
            QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, ReadResponse,
            WriteRequest, WriteResponse, byte_stream_server::ByteStream,
        },
    },
    status,
};
use crate::{
    cache::{Admission, Cache, CacheError, upload_error},
    store,
};

/// The largest batch request or response, and the largest message, apart from framing. The
/// gRPC default, so clients send anything bigger over `ByteStream`.
pub(crate) const MAX_BATCH_BYTES: u64 = 4 * 1024 * 1024;

/// The largest message decoded or encoded: a full batch plus room for its digests and framing.
pub const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024 + 256 * 1024;

/// The largest chunk sent by `ByteStream.Read`.
const READ_CHUNK: usize = 1024 * 1024;

/// Existence checks in flight at once for one request.
const LOOKUP_CONCURRENCY: usize = 32;

/// The services, sharing one cache.
#[derive(Debug, Clone)]
pub struct Services {
    cache: Arc<Cache>,
    /// How long an upload may send nothing before it fails.
    idle_timeout: Duration,
}

impl Services {
    /// Services over `cache`. An upload that sends nothing for `idle_timeout` fails.
    #[must_use]
    pub fn new(cache: Arc<Cache>, idle_timeout: Duration) -> Self {
        Self {
            cache,
            idle_timeout,
        }
    }

    /// Reads a whole entry, or `None` on a miss or when its stored size is not `expected`.
    async fn read_all(&self, blob: &Blob) -> Result<Option<Bytes>, Status> {
        let Some(hit) = self
            .cache
            .get(&blob.key)
            .await
            .map_err(|e| status::storage(&e))?
        else {
            return Ok(None);
        };
        if hit.blob.size != blob.size {
            tracing::warn!(key = %blob.key, stored = hit.blob.size, "size differs from digest");
            return Ok(None);
        }
        let mut data = BytesMut::with_capacity(usize::try_from(hit.blob.size).unwrap_or(0));
        let mut body = hit.blob.body;
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| status::storage(&CacheError::from(e)))?;
            data.extend_from_slice(&chunk);
        }
        Ok(Some(data.freeze()))
    }

    /// Whether a blob is present with its digest's size. The empty blob always is.
    async fn present(&self, blob: &Blob) -> Result<bool, Status> {
        if blob.is_empty() {
            return Ok(true);
        }
        let size = self
            .cache
            .head(&blob.key)
            .await
            .map_err(|e| status::storage(&e))?;
        Ok(size == Some(blob.size))
    }

    /// The distinct blobs among `blobs` that are not present.
    async fn missing(&self, blobs: Vec<Blob>) -> Result<Vec<Blob>, Status> {
        let mut seen = HashSet::new();
        let distinct: Vec<Blob> = blobs
            .into_iter()
            .filter(|b| seen.insert(b.clone()))
            .collect();
        stream::iter(distinct)
            .map(|blob| async move { self.present(&blob).await.map(|ok| (!ok).then_some(blob)) })
            .buffered(LOOKUP_CONCURRENCY)
            .try_filter_map(|missing| async move { Ok(missing) })
            .try_collect()
            .await
    }

    /// Stores a blob held in memory, after checking it hashes to its digest.
    async fn store_blob(&self, blob: &Blob, data: Bytes) -> Result<(), Status> {
        if data.len() as u64 != blob.size {
            return Err(status::invalid_argument(
                "SIZE_MISMATCH",
                format!(
                    "The data is {} bytes; the digest says {}.",
                    data.len(),
                    blob.size
                ),
            ));
        }
        if Sha256::digest(&data).as_slice() != blob.sha256() {
            return Err(status::digest_mismatch(&blob.key));
        }
        if blob.is_empty() {
            return Ok(());
        }
        self.store(
            blob,
            Some(blob.size),
            stream::once(async move { Ok(data) }).boxed(),
        )
        .await
    }

    /// Stores an entry, refusing it as too large before reading anything when its size says so.
    async fn store(
        &self,
        blob: &Blob,
        size: Option<u64>,
        body: store::ByteStream,
    ) -> Result<(), Status> {
        match self.cache.admit(&blob.key, size) {
            Admission::Accept => {}
            Admission::Reject(e) => return Err(status::storage(&e)),
            // Bazel namespaces always refuse; never dropped.
            Admission::Drop => return Ok(()),
        }
        self.cache
            .put(&blob.key, size, body)
            .await
            .map(|_| ())
            .map_err(|e| upload_status(blob, &e))
    }
}

/// Runs a call and counts it by method and status code.
async fn observe<T>(
    method: &'static str,
    call: impl Future<Output = Result<T, Status>>,
) -> Result<T, Status> {
    let result = call.await;
    let code = match &result {
        Ok(_) => Code::Ok,
        Err(status) => status.code(),
    };
    metrics::counter!(
        "mammoth_grpc_requests_total",
        "method" => method,
        "code" => code_label(code),
    )
    .increment(1);
    result
}

fn code_label(code: Code) -> &'static str {
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

fn to_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn api_version(minor: i32) -> SemVer {
    SemVer {
        major: 2,
        minor,
        patch: 0,
        prerelease: String::new(),
    }
}

#[tonic::async_trait]
impl Capabilities for Services {
    async fn get_capabilities(
        &self,
        _request: Request<GetCapabilitiesRequest>,
    ) -> Result<Response<ServerCapabilities>, Status> {
        observe("GetCapabilities", async {
            let sha256 = digest_function::Value::Sha256 as i32;
            Ok(Response::new(ServerCapabilities {
                cache_capabilities: Some(CacheCapabilities {
                    digest_functions: vec![sha256],
                    action_cache_update_capabilities: Some(ActionCacheUpdateCapabilities {
                        update_enabled: true,
                    }),
                    max_batch_total_size_bytes: to_i64(MAX_BATCH_BYTES),
                    symlink_absolute_path_strategy: symlink_absolute_path_strategy::Value::Allowed
                        as i32,
                    max_cas_blob_size_bytes: to_i64(self.cache.max_entry_size()),
                    ..CacheCapabilities::default()
                }),
                execution_capabilities: Some(ExecutionCapabilities {
                    digest_function: sha256,
                    digest_functions: vec![sha256],
                    exec_enabled: false,
                    ..ExecutionCapabilities::default()
                }),
                deprecated_api_version: None,
                low_api_version: Some(api_version(0)),
                high_api_version: Some(api_version(3)),
            }))
        })
        .await
    }
}

/// The CAS blobs an action result points at: its output files, output directory trees and
/// captured output streams. Only the direct references; trees are not walked.
fn referenced_blobs(result: &ActionResult) -> Result<Vec<Blob>, Status> {
    let digests = result
        .output_files
        .iter()
        .map(|f| f.digest.as_ref())
        .chain(
            result
                .output_directories
                .iter()
                .map(|d| d.tree_digest.as_ref()),
        )
        .flatten()
        .chain(result.stdout_digest.as_ref())
        .chain(result.stderr_digest.as_ref());
    digests.map(|d| Blob::cas(Some(d))).collect()
}

#[tonic::async_trait]
impl ActionCache for Services {
    async fn get_action_result(
        &self,
        request: Request<GetActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        observe("GetActionResult", async {
            let request = request.into_inner();
            digest::check_function(request.digest_function)?;
            let action = Blob::action(request.action_digest.as_ref())?;
            // The action's own size is not the result's: read whatever is stored.
            let Some(hit) = self
                .cache
                .get(&action.key)
                .await
                .map_err(|e| status::storage(&e))?
            else {
                return Err(status::not_found(&action.key));
            };
            let data: Vec<Bytes> = hit
                .blob
                .body
                .try_collect()
                .await
                .map_err(|e| status::storage(&CacheError::from(e)))?;
            let Ok(result) = ActionResult::decode(data.concat().as_slice()) else {
                tracing::warn!(key = %action.key, "stored action result does not decode; a miss");
                return Err(status::not_found(&action.key));
            };
            // A result whose outputs are gone would fail a client that trusts it, such as Bazel
            // with --remote_download_minimal: answer a miss so the action runs again.
            let Ok(outputs) = referenced_blobs(&result) else {
                tracing::warn!(key = %action.key, "stored action result has bad digests; a miss");
                return Err(status::not_found(&action.key));
            };
            if !self.missing(outputs).await?.is_empty() {
                tracing::debug!(key = %action.key, "action result outputs missing; a miss");
                return Err(status::not_found(&action.key));
            }
            Ok(Response::new(result))
        })
        .await
    }

    async fn update_action_result(
        &self,
        request: Request<UpdateActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        observe("UpdateActionResult", async {
            let request = request.into_inner();
            digest::check_function(request.digest_function)?;
            let action = Blob::action(request.action_digest.as_ref())?;
            let result = request.action_result.ok_or_else(|| {
                status::invalid_argument("ACTION_RESULT_MISSING", "The action result is missing.")
            })?;
            referenced_blobs(&result)?;
            let data = Bytes::from(result.encode_to_vec());
            let size = data.len() as u64;
            self.store(
                &action,
                Some(size),
                stream::once(async move { Ok(data) }).boxed(),
            )
            .await?;
            Ok(Response::new(result))
        })
        .await
    }
}

/// Validates a batch read's digests, refusing the whole call if they add up to more than a batch
/// may carry.
fn batch_blobs(digests: &[Digest]) -> Result<Vec<Blob>, Status> {
    let blobs = digests
        .iter()
        .map(|d| Blob::cas(Some(d)))
        .collect::<Result<Vec<_>, _>>()?;
    let total: u64 = blobs.iter().map(|b| b.size).sum();
    if total > MAX_BATCH_BYTES {
        return Err(status::invalid_argument(
            "BATCH_TOO_LARGE",
            format!(
                "The batch is {total} bytes; max_batch_total_size_bytes is {MAX_BATCH_BYTES}. \
                 Use ByteStream for larger blobs."
            ),
        ));
    }
    Ok(blobs)
}

#[tonic::async_trait]
impl ContentAddressableStorage for Services {
    async fn find_missing_blobs(
        &self,
        request: Request<FindMissingBlobsRequest>,
    ) -> Result<Response<FindMissingBlobsResponse>, Status> {
        observe("FindMissingBlobs", async {
            let request = request.into_inner();
            digest::check_function(request.digest_function)?;
            let blobs = request
                .blob_digests
                .iter()
                .map(|d| Blob::cas(Some(d)))
                .collect::<Result<Vec<_>, _>>()?;
            let missing = self.missing(blobs).await?;
            Ok(Response::new(FindMissingBlobsResponse {
                missing_blob_digests: missing.iter().map(Blob::digest).collect(),
            }))
        })
        .await
    }

    async fn batch_update_blobs(
        &self,
        request: Request<BatchUpdateBlobsRequest>,
    ) -> Result<Response<BatchUpdateBlobsResponse>, Status> {
        observe("BatchUpdateBlobs", async {
            let request = request.into_inner();
            digest::check_function(request.digest_function)?;
            // The message size limit bounds the batch; each blob's digest is checked on its own.
            let responses = stream::iter(request.requests)
                .map(|item| async move {
                    let outcome = match Blob::cas(item.digest.as_ref()) {
                        Err(e) => Err(e),
                        Ok(_) if item.compressor != 0 => Err(status::invalid_argument(
                            "UNSUPPORTED_COMPRESSOR",
                            "Compressed blobs are not supported.",
                        )),
                        Ok(blob) => self.store_blob(&blob, item.data).await,
                    };
                    batch_update_blobs_response::Response {
                        digest: item.digest,
                        status: Some(
                            outcome.map_or_else(|e| status::to_rpc(&e), |()| status::ok()),
                        ),
                    }
                })
                .buffered(LOOKUP_CONCURRENCY)
                .collect()
                .await;
            Ok(Response::new(BatchUpdateBlobsResponse { responses }))
        })
        .await
    }

    async fn batch_read_blobs(
        &self,
        request: Request<BatchReadBlobsRequest>,
    ) -> Result<Response<BatchReadBlobsResponse>, Status> {
        observe("BatchReadBlobs", async {
            let request = request.into_inner();
            digest::check_function(request.digest_function)?;
            let blobs = batch_blobs(&request.digests)?;
            let responses = stream::iter(blobs)
                .map(|blob| async move {
                    let read = if blob.is_empty() {
                        Ok(Some(Bytes::new()))
                    } else {
                        self.read_all(&blob).await
                    };
                    let (data, outcome) = match read {
                        Ok(Some(data)) => (data, status::ok()),
                        Ok(None) => (Bytes::new(), status::to_rpc(&status::not_found(&blob.key))),
                        Err(e) => (Bytes::new(), status::to_rpc(&e)),
                    };
                    batch_read_blobs_response::Response {
                        digest: Some(blob.digest()),
                        data,
                        compressor: 0,
                        status: Some(outcome),
                    }
                })
                .buffered(LOOKUP_CONCURRENCY)
                .collect()
                .await;
            Ok(Response::new(BatchReadBlobsResponse { responses }))
        })
        .await
    }

    type GetTreeStream = Pin<Box<dyn Stream<Item = Result<GetTreeResponse, Status>> + Send>>;

    async fn get_tree(
        &self,
        _request: Request<GetTreeRequest>,
    ) -> Result<Response<Self::GetTreeStream>, Status> {
        observe("GetTree", async { Err(unimplemented("GetTree")) }).await
    }

    async fn split_blob(
        &self,
        _request: Request<SplitBlobRequest>,
    ) -> Result<Response<SplitBlobResponse>, Status> {
        observe("SplitBlob", async { Err(unimplemented("SplitBlob")) }).await
    }

    async fn splice_blob(
        &self,
        _request: Request<SpliceBlobRequest>,
    ) -> Result<Response<SpliceBlobResponse>, Status> {
        observe("SpliceBlob", async { Err(unimplemented("SpliceBlob")) }).await
    }
}

fn unimplemented(method: &str) -> Status {
    status::error(
        Code::Unimplemented,
        "METHOD_NOT_SUPPORTED",
        format!("{method} is not supported by this cache."),
        &[],
    )
}

#[tonic::async_trait]
impl ByteStream for Services {
    type ReadStream = Pin<Box<dyn Stream<Item = Result<ReadResponse, Status>> + Send>>;

    async fn read(
        &self,
        request: Request<ReadRequest>,
    ) -> Result<Response<Self::ReadStream>, Status> {
        observe("Read", async {
            let request = request.into_inner();
            let blob = digest::read_resource(&request.resource_name)?;
            let offset = u64::try_from(request.read_offset)
                .ok()
                .filter(|&offset| offset <= blob.size)
                .ok_or_else(|| {
                    status::error(
                        Code::OutOfRange,
                        "OFFSET_OUT_OF_RANGE",
                        "read_offset is outside the blob.",
                        &[],
                    )
                })?;
            let limit = u64::try_from(request.read_limit).map_err(|_| {
                status::invalid_argument("INVALID_READ_LIMIT", "read_limit is negative.")
            })?;
            let wanted = match limit {
                0 => blob.size - offset,
                limit => limit.min(blob.size - offset),
            };
            if blob.is_empty() || wanted == 0 {
                let empty: Self::ReadStream = Box::pin(stream::empty());
                return Ok(Response::new(empty));
            }
            let hit = self
                .cache
                .get(&blob.key)
                .await
                .map_err(|e| status::storage(&e))?
                .ok_or_else(|| status::not_found(&blob.key))?;
            if hit.blob.size != blob.size {
                tracing::warn!(key = %blob.key, stored = hit.blob.size, "size differs from digest");
                return Err(status::not_found(&blob.key));
            }
            let chunks: Self::ReadStream =
                Box::pin(window(hit.blob.body, offset, wanted).map(|chunk| {
                    chunk
                        .map(|data| ReadResponse { data })
                        .map_err(|e| status::storage(&CacheError::from(e)))
                }));
            Ok(Response::new(chunks))
        })
        .await
    }

    async fn write(
        &self,
        request: Request<Streaming<WriteRequest>>,
    ) -> Result<Response<WriteResponse>, Status> {
        observe("Write", async {
            let mut messages = request.into_inner();
            let first = tokio::time::timeout(self.idle_timeout, messages.message())
                .await
                .map_err(|_| idle())??
                .ok_or_else(|| {
                    status::invalid_argument("EMPTY_WRITE", "The write sent nothing.")
                })?;
            let blob = digest::write_resource(&first.resource_name)?;
            let committed = to_i64(blob.size);
            if blob.is_empty() {
                return Ok(Response::new(WriteResponse { committed_size: 0 }));
            }
            let body = Upload::new(first, messages, &blob, self.idle_timeout).into_stream();
            // A blob already present answers at once with its full size, which tells the client
            // to stop sending.
            self.store(&blob, Some(blob.size), body).await?;
            Ok(Response::new(WriteResponse {
                committed_size: committed,
            }))
        })
        .await
    }

    async fn query_write_status(
        &self,
        request: Request<QueryWriteStatusRequest>,
    ) -> Result<Response<QueryWriteStatusResponse>, Status> {
        observe("QueryWriteStatus", async {
            let blob = digest::write_resource(&request.into_inner().resource_name)?;
            // Uploads are not resumable: a blob is all there or not at all.
            let complete = self.present(&blob).await?;
            Ok(Response::new(QueryWriteStatusResponse {
                committed_size: if complete { to_i64(blob.size) } else { 0 },
                complete,
            }))
        })
        .await
    }
}

/// The part of `body` from `offset`, `len` bytes long, in chunks of at most [`READ_CHUNK`].
fn window(
    body: store::ByteStream,
    offset: u64,
    len: u64,
) -> impl Stream<Item = io::Result<Bytes>> + Send {
    struct Window {
        body: store::ByteStream,
        skip: u64,
        left: u64,
        pending: Bytes,
    }
    let state = Window {
        body,
        skip: offset,
        left: len,
        pending: Bytes::new(),
    };
    stream::try_unfold(state, |mut state| async move {
        loop {
            if state.left == 0 {
                return Ok(None);
            }
            if !state.pending.is_empty() {
                let take = state
                    .pending
                    .len()
                    .min(READ_CHUNK)
                    .min(usize::try_from(state.left).unwrap_or(usize::MAX));
                let chunk = state.pending.split_to(take);
                state.left -= chunk.len() as u64;
                return Ok(Some((chunk, state)));
            }
            let Some(mut chunk) = state.body.next().await.transpose()? else {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "entry ended before its size",
                ));
            };
            let skip = usize::try_from(state.skip)
                .unwrap_or(usize::MAX)
                .min(chunk.len());
            let _ = chunk.split_to(skip);
            state.skip -= skip as u64;
            state.pending = chunk;
        }
    })
}

fn idle() -> Status {
    status::error(
        Code::DeadlineExceeded,
        "UPLOAD_IDLE",
        "The upload sent nothing for too long.",
        &[],
    )
}

/// Why an upload stream failed, carried inside the `io::Error` the core reports.
#[derive(Debug)]
enum UploadError {
    /// The content does not hash to the digest.
    Mismatch,
    /// The client broke the `ByteStream` protocol.
    Protocol(String),
    /// The client sent nothing for the idle timeout.
    Idle,
    /// The client's stream failed, usually because it was cancelled.
    Client(Status),
}

impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mismatch => f.write_str("content does not hash to its digest"),
            Self::Protocol(message) => f.write_str(message),
            Self::Idle => f.write_str("the upload sent nothing for too long"),
            Self::Client(status) => write!(f, "the client's stream failed: {status}"),
        }
    }
}

impl std::error::Error for UploadError {}

/// The status for a failed store, finding an [`UploadError`] inside the storage error.
fn upload_status(blob: &Blob, error: &CacheError) -> Status {
    let upload = match error {
        // `cache::upload_error` wraps the `UploadError`; it is the wrapper's source.
        CacheError::Upload(e) => e
            .get_ref()
            .and_then(|e| std::error::Error::source(e))
            .and_then(|e| e.downcast_ref::<UploadError>()),
        CacheError::TooLarge { .. } | CacheError::Unavailable(_) | CacheError::Internal(_) => None,
    };
    match upload {
        Some(UploadError::Mismatch) => status::digest_mismatch(&blob.key),
        Some(UploadError::Protocol(message)) => {
            status::invalid_argument("INVALID_WRITE", message.clone())
        }
        Some(UploadError::Idle) => idle(),
        Some(UploadError::Client(status)) => status.clone(),
        None => status::storage(error),
    }
}

/// A `ByteStream.Write` call's messages as a body for the core, checked as they arrive: offsets
/// must follow on, the bytes must not pass the digest's size, and they must hash to the digest.
///
/// The check runs as the last byte arrives, before that chunk is passed on, because a tier that
/// knows the size may never poll for the end of the stream (as in the HTTP adapter). After the
/// last byte the stream ends without reading the client's closing message.
struct Upload {
    first: Option<WriteRequest>,
    rest: Streaming<WriteRequest>,
    resource: String,
    size: u64,
    expected: [u8; 32],
    received: u64,
    hasher: Sha256,
    idle_timeout: Duration,
}

impl Upload {
    fn new(
        first: WriteRequest,
        rest: Streaming<WriteRequest>,
        blob: &Blob,
        idle_timeout: Duration,
    ) -> Self {
        Self {
            resource: first.resource_name.clone(),
            first: Some(first),
            rest,
            size: blob.size,
            expected: blob.sha256(),
            received: 0,
            hasher: Sha256::new(),
            idle_timeout,
        }
    }

    fn into_stream(self) -> store::ByteStream {
        stream::try_unfold(self, |mut upload| async move {
            let fail = |e: UploadError| upload_error(e);
            loop {
                if upload.received == upload.size {
                    return Ok(None);
                }
                let message = match upload.first.take() {
                    Some(first) => first,
                    None => match tokio::time::timeout(upload.idle_timeout, upload.rest.message())
                        .await
                    {
                        Err(_) => return Err(fail(UploadError::Idle)),
                        Ok(Err(status)) => return Err(fail(UploadError::Client(status))),
                        Ok(Ok(None)) => {
                            return Err(fail(UploadError::Protocol(format!(
                                "The write ended after {} of {} bytes.",
                                upload.received, upload.size
                            ))));
                        }
                        Ok(Ok(Some(message))) => message,
                    },
                };
                if !message.resource_name.is_empty() && message.resource_name != upload.resource {
                    return Err(fail(UploadError::Protocol(
                        "The resource name changed during the write.".to_owned(),
                    )));
                }
                if u64::try_from(message.write_offset).ok() != Some(upload.received) {
                    return Err(fail(UploadError::Protocol(format!(
                        "write_offset is {}; expected {}.",
                        message.write_offset, upload.received
                    ))));
                }
                let data = message.data;
                upload.received += data.len() as u64;
                if upload.received > upload.size {
                    return Err(fail(UploadError::Protocol(format!(
                        "The write sent more than the digest's {} bytes.",
                        upload.size
                    ))));
                }
                upload.hasher.update(&data);
                if upload.received == upload.size {
                    let hasher = std::mem::take(&mut upload.hasher);
                    if hasher.finalize().as_slice() != upload.expected {
                        return Err(fail(UploadError::Mismatch));
                    }
                } else if message.finish_write {
                    return Err(fail(UploadError::Protocol(format!(
                        "finish_write after {} of {} bytes.",
                        upload.received, upload.size
                    ))));
                }
                if !data.is_empty() {
                    return Ok(Some((data, upload)));
                }
            }
        })
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(chunks: &[&'static [u8]]) -> store::ByteStream {
        stream::iter(
            chunks
                .iter()
                .map(|c| Ok(Bytes::from_static(c)))
                .collect::<Vec<_>>(),
        )
        .boxed()
    }

    async fn collect(s: impl Stream<Item = io::Result<Bytes>>) -> Vec<u8> {
        let parts: Vec<Bytes> = s.try_collect().await.unwrap();
        parts.concat()
    }

    #[tokio::test]
    async fn window_skips_and_limits() {
        let data = body(&[b"hello ", b"wor", b"ld"]);
        assert_eq!(collect(window(data, 4, 5)).await, b"o wor");
        let data = body(&[b"hello ", b"wor", b"ld"]);
        assert_eq!(collect(window(data, 0, 11)).await, b"hello world");
        let data = body(&[b"hello"]);
        assert_eq!(collect(window(data, 5, 0)).await, b"");
    }

    #[tokio::test]
    async fn window_reports_a_short_entry() {
        let result: Result<Vec<Bytes>, _> = window(body(&[b"abc"]), 0, 5).try_collect().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn window_splits_large_chunks() {
        let big = Bytes::from(vec![7_u8; READ_CHUNK * 2 + 10]);
        let data = stream::once(async move { Ok(big) }).boxed();
        let parts: Vec<Bytes> = window(data, 0, (READ_CHUNK * 2 + 10) as u64)
            .try_collect()
            .await
            .unwrap();
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.len() <= READ_CHUNK));
    }

    mod properties {
        use proptest::prelude::*;

        use super::*;

        proptest! {
            #[test]
            fn window_is_a_slice(
                data in proptest::collection::vec(any::<u8>(), 0..4096),
                cuts in proptest::collection::vec(any::<prop::sample::Index>(), 0..8),
                start in any::<prop::sample::Index>(),
                len in any::<prop::sample::Index>(),
            ) {
                // Any split of the entry into chunks, any window inside it.
                let mut points: Vec<usize> = cuts.iter().map(|c| c.index(data.len() + 1)).collect();
                points.extend([0, data.len()]);
                points.sort_unstable();
                let chunks: Vec<io::Result<Bytes>> = points
                    .windows(2)
                    .map(|w| Ok(Bytes::copy_from_slice(&data[w[0]..w[1]])))
                    .collect();
                let offset = start.index(data.len() + 1);
                let len = len.index(data.len() - offset + 1);
                let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
                let got = runtime.block_on(collect(window(
                    stream::iter(chunks).boxed(),
                    offset as u64,
                    len as u64,
                )));
                prop_assert_eq!(got.as_slice(), &data[offset..offset + len]);
            }
        }
    }
}
