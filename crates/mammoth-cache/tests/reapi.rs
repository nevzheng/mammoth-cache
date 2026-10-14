//! The REAPI cache over gRPC (design doc 0016), served on a real socket and called with tonic
//! clients, the way Bazel, Buck2, Pants, sbt 2 and Please call it.

// Helpers panic on unexpected values, like the tests that call them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{fmt::Write, net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    body::Body,
    http::{Request as HttpRequest, StatusCode},
};
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use http_body_util::BodyExt;
use mammoth_cache::{
    auth::{Authenticator, Scope, Scopes, hash_token},
    cache::Cache,
    config::Protocols,
    key::{CacheKey, Namespace},
    reapi::{
        self,
        proto::{
            build::bazel::remote::execution::v2::{
                ActionResult, BatchReadBlobsRequest, BatchUpdateBlobsRequest,
                BatchUpdateBlobsResponse, Digest, Directory, ExecuteRequest, FileNode,
                FindMissingBlobsRequest, GetActionResultRequest, GetCapabilitiesRequest,
                GetTreeRequest, OutputDirectory, OutputFile, Tree, UpdateActionResultRequest,
                action_cache_client::ActionCacheClient, batch_update_blobs_request,
                capabilities_client::CapabilitiesClient,
                content_addressable_storage_client::ContentAddressableStorageClient,
                digest_function, execution_client::ExecutionClient,
            },
            google::bytestream::{
                QueryWriteStatusRequest, ReadRequest, WriteRequest,
                byte_stream_client::ByteStreamClient,
            },
        },
    },
    router_with,
    server::ServerSettings,
    store::memory::{FoyerBackend, MemoryConfig, MemoryStore},
};
use prost::Message;
use sha2::Sha256;
use tokio::net::TcpListener;
use tonic::{
    Code, Request, Status,
    metadata::MetadataValue,
    transport::{Channel, Endpoint},
};
use tonic_types::{ErrorInfo, RpcStatusExt, StatusExt};
use tower::ServiceExt;

const LIMIT: u64 = 1 << 20;

struct Node {
    cache: Arc<Cache>,
    channel: Channel,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Node {
    async fn start(auth: Authenticator) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        Self::serve(auth, listener, addr).await
    }

    /// Serves on `listener`, with clients connecting to `addr`.
    async fn serve(auth: Authenticator, listener: TcpListener, addr: SocketAddr) -> Self {
        Self::serve_with(auth, listener, addr, LIMIT, settings()).await
    }

    /// An open node with entries up to `limit` bytes and the given timeouts.
    async fn with(limit: u64, settings: ServerSettings) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        Self::serve_with(Authenticator::open(), listener, addr, limit, settings).await
    }

    async fn serve_with(
        auth: Authenticator,
        listener: TcpListener,
        addr: SocketAddr,
        limit: u64,
        settings: ServerSettings,
    ) -> Self {
        let cache = Arc::new(
            Cache::builder()
                .bounded_tier(Arc::new(
                    MemoryStore::<FoyerBackend>::with_config(MemoryConfig {
                        max_entry_bytes: limit,
                        ..MemoryConfig::new(64 << 20)
                    })
                    .unwrap(),
                ))
                .max_entry_size(limit)
                .build(),
        );
        let routes = reapi::routes(cache.clone(), Arc::new(auth), settings);
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(reapi::serve(listener, None, routes, settings, async {
            let _ = stopped.await;
        }));
        let channel = Endpoint::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        Self {
            cache,
            channel,
            stop: Some(stop),
            server,
        }
    }

    async fn open() -> Self {
        Self::start(Authenticator::open()).await
    }

    fn cas(&self) -> ContentAddressableStorageClient<Channel> {
        ContentAddressableStorageClient::new(self.channel.clone())
    }

    fn ac(&self) -> ActionCacheClient<Channel> {
        ActionCacheClient::new(self.channel.clone())
    }

    fn bytestream(&self) -> ByteStreamClient<Channel> {
        ByteStreamClient::new(self.channel.clone())
    }
}

fn settings() -> ServerSettings {
    ServerSettings {
        shutdown_delay: Duration::ZERO,
        shutdown_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(5),
        body_read_timeout: Duration::from_secs(5),
        send_timeout: Duration::from_secs(5),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        write!(s, "{b:02x}").unwrap();
        s
    })
}

fn digest_of(data: &[u8]) -> Digest {
    use sha2::Digest as _;
    Digest {
        hash: hex(&Sha256::digest(data)),
        size_bytes: i64::try_from(data.len()).unwrap(),
    }
}

/// The status's `ErrorInfo`, which every error carries in the `mammoth-cache.dev` domain.
fn info(status: &Status) -> ErrorInfo {
    let info = status.get_details_error_info().unwrap();
    assert_eq!(info.domain, "mammoth-cache.dev");
    info
}

fn reason(status: &Status) -> String {
    info(status).reason
}

async fn upload(node: &Node, blobs: &[&[u8]]) {
    let response = node
        .cas()
        .batch_update_blobs(BatchUpdateBlobsRequest {
            requests: blobs
                .iter()
                .map(|b| batch_update_blobs_request::Request {
                    digest: Some(digest_of(b)),
                    data: Bytes::copy_from_slice(b),
                    compressor: 0,
                })
                .collect(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    for r in response.responses {
        assert_eq!(r.status.unwrap().code, 0);
    }
}

#[tokio::test]
async fn capabilities_describe_a_cache_without_execution() {
    let node = Node::open().await;
    let caps = CapabilitiesClient::new(node.channel.clone())
        .get_capabilities(GetCapabilitiesRequest::default())
        .await
        .unwrap()
        .into_inner();
    let cache = caps.cache_capabilities.unwrap();
    assert_eq!(
        cache.digest_functions,
        [digest_function::Value::Sha256 as i32]
    );
    assert!(
        cache
            .action_cache_update_capabilities
            .unwrap()
            .update_enabled
    );
    // Room for each blob's digest and status under a client's 4 MiB message limit.
    assert_eq!(cache.max_batch_total_size_bytes, (4 << 20) - (256 << 10));
    assert_eq!(cache.max_cas_blob_size_bytes, i64::try_from(LIMIT).unwrap());
    assert!(cache.supported_compressors.is_empty());
    assert!(!caps.execution_capabilities.unwrap().exec_enabled);
    assert_eq!(caps.low_api_version.unwrap().major, 2);
    assert_eq!(caps.high_api_version.unwrap().minor, 3);
}

#[tokio::test]
async fn execution_is_not_served() {
    let node = Node::open().await;
    let err = ExecutionClient::new(node.channel.clone())
        .execute(ExecuteRequest::default())
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unimplemented);
    let err = node
        .cas()
        .get_tree(GetTreeRequest::default())
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unimplemented);
}

#[tokio::test]
async fn batch_upload_then_find_and_read() {
    let node = Node::open().await;
    let (a, b): (&[u8], &[u8]) = (b"alpha", b"bravo");
    let empty = digest_of(b"");
    let missing = node
        .cas()
        .find_missing_blobs(FindMissingBlobsRequest {
            blob_digests: vec![digest_of(a), digest_of(b), digest_of(a), empty.clone()],
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner()
        .missing_blob_digests;
    // Deduplicated, and the empty blob is always present.
    assert_eq!(missing, [digest_of(a), digest_of(b)]);

    upload(&node, &[a]).await;
    let missing = node
        .cas()
        .find_missing_blobs(FindMissingBlobsRequest {
            blob_digests: vec![digest_of(a), digest_of(b)],
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner()
        .missing_blob_digests;
    assert_eq!(missing, [digest_of(b)]);

    let read = node
        .cas()
        .batch_read_blobs(BatchReadBlobsRequest {
            digests: vec![digest_of(a), digest_of(b), empty],
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner()
        .responses;
    assert_eq!(read[0].data, a);
    assert_eq!(read[0].status.as_ref().unwrap().code, 0);
    assert_eq!(read[1].status.as_ref().unwrap().code, Code::NotFound as i32);
    assert_eq!(read[2].status.as_ref().unwrap().code, 0);
    assert!(read[2].data.is_empty());
}

#[tokio::test]
async fn batch_upload_checks_each_blob() {
    let node = Node::open().await;
    let wrong = Digest {
        hash: digest_of(b"other").hash,
        size_bytes: 5,
    };
    let response = node
        .cas()
        .batch_update_blobs(BatchUpdateBlobsRequest {
            requests: vec![
                batch_update_blobs_request::Request {
                    digest: Some(digest_of(b"fine")),
                    data: Bytes::from_static(b"fine"),
                    compressor: 0,
                },
                batch_update_blobs_request::Request {
                    digest: Some(wrong.clone()),
                    data: Bytes::from_static(b"bytes"),
                    compressor: 0,
                },
                batch_update_blobs_request::Request {
                    digest: Some(digest_of(b"short")),
                    data: Bytes::from_static(b"shor"),
                    compressor: 0,
                },
                batch_update_blobs_request::Request {
                    digest: Some(Digest {
                        hash: "XYZ".to_owned(),
                        size_bytes: 1,
                    }),
                    data: Bytes::from_static(b"x"),
                    compressor: 0,
                },
            ],
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner()
        .responses;
    let codes: Vec<i32> = response
        .iter()
        .map(|r| r.status.as_ref().unwrap().code)
        .collect();
    let invalid = Code::InvalidArgument as i32;
    assert_eq!(codes, [0, invalid, invalid, invalid]);
    // Each failed blob carries the same `ErrorInfo` it would fail a call with.
    let reasons: Vec<String> = response[1..]
        .iter()
        .map(|r| {
            let info = r.status.as_ref().unwrap().get_details_error_info().unwrap();
            assert_eq!(info.domain, "mammoth-cache.dev");
            info.reason
        })
        .collect();
    assert_eq!(
        reasons,
        ["DIGEST_MISMATCH", "SIZE_MISMATCH", "INVALID_DIGEST"]
    );
    let size_mismatch = response[2]
        .status
        .as_ref()
        .unwrap()
        .get_details_error_info()
        .unwrap();
    assert_eq!(
        size_mismatch.metadata["key"],
        digest_of(b"short").hash,
        "the bare digest"
    );
    // The mismatched blob was not stored under the digest it claimed.
    let missing = node
        .cas()
        .find_missing_blobs(FindMissingBlobsRequest {
            blob_digests: vec![wrong.clone()],
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner()
        .missing_blob_digests;
    assert_eq!(missing, [wrong]);
}

#[tokio::test]
async fn batches_are_bounded() {
    let node = Node::open().await;
    let big = Digest {
        hash: "a".repeat(64),
        size_bytes: 5 << 20,
    };
    let err = node
        .cas()
        .batch_read_blobs(BatchReadBlobsRequest {
            digests: vec![big],
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    assert_eq!(reason(&err), "BATCH_TOO_LARGE");
}

#[tokio::test]
async fn rejects_other_digest_functions() {
    let node = Node::open().await;
    let err = node
        .cas()
        .find_missing_blobs(FindMissingBlobsRequest {
            blob_digests: vec![],
            digest_function: digest_function::Value::Blake3 as i32,
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
}

fn write_requests(resource: &str, data: &[u8], chunk: usize) -> Vec<WriteRequest> {
    let mut requests = Vec::new();
    let mut offset = 0;
    for (i, part) in data.chunks(chunk).enumerate() {
        requests.push(WriteRequest {
            resource_name: if i == 0 {
                resource.to_owned()
            } else {
                String::new()
            },
            write_offset: offset,
            finish_write: offset + i64::try_from(part.len()).unwrap()
                == i64::try_from(data.len()).unwrap(),
            data: Bytes::copy_from_slice(part),
        });
        offset += i64::try_from(part.len()).unwrap();
    }
    requests
}

fn upload_name(digest: &Digest) -> String {
    format!(
        "instance/uploads/7d3f1a2e-1111-4c2b-9a8e-123456789abc/blobs/{}/{}",
        digest.hash, digest.size_bytes
    )
}

async fn read(node: &Node, digest: &Digest, offset: i64, limit: i64) -> Result<Vec<u8>, Status> {
    let mut stream = node
        .bytestream()
        .read(ReadRequest {
            resource_name: format!("instance/blobs/{}/{}", digest.hash, digest.size_bytes),
            read_offset: offset,
            read_limit: limit,
        })
        .await?
        .into_inner();
    let mut out = Vec::new();
    while let Some(message) = stream.message().await? {
        out.extend_from_slice(&message.data);
    }
    Ok(out)
}

#[tokio::test]
async fn bytestream_round_trip() {
    let node = Node::open().await;
    let data: Vec<u8> = (0..300_000_u32).map(|i| (i % 251) as u8).collect();
    let digest = digest_of(&data);
    let committed = node
        .bytestream()
        .write(stream::iter(write_requests(
            &upload_name(&digest),
            &data,
            64 << 10,
        )))
        .await
        .unwrap()
        .into_inner()
        .committed_size;
    assert_eq!(committed, digest.size_bytes);

    assert_eq!(read(&node, &digest, 0, 0).await.unwrap(), data);
    assert_eq!(
        read(&node, &digest, 1000, 10).await.unwrap(),
        &data[1000..1010]
    );
    assert_eq!(
        read(&node, &digest, 299_990, 0).await.unwrap(),
        &data[299_990..]
    );
    let err = read(&node, &digest, 300_001, 0).await.unwrap_err();
    assert_eq!(err.code(), Code::OutOfRange);

    let status = node
        .bytestream()
        .query_write_status(QueryWriteStatusRequest {
            resource_name: upload_name(&digest),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(status.complete);
    assert_eq!(status.committed_size, digest.size_bytes);

    // A second upload of the same blob is answered at once.
    let committed = node
        .bytestream()
        .write(stream::iter(write_requests(
            &upload_name(&digest),
            &data,
            64 << 10,
        )))
        .await
        .unwrap()
        .into_inner()
        .committed_size;
    assert_eq!(committed, digest.size_bytes);
}

#[tokio::test]
async fn bytestream_read_misses() {
    let node = Node::open().await;
    let err = read(&node, &digest_of(b"nothing here"), 0, 0)
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
    let not_found = info(&err);
    assert_eq!(not_found.reason, "ENTRY_NOT_FOUND");
    assert_eq!(not_found.metadata["key"], digest_of(b"nothing here").hash);
    let status = node
        .bytestream()
        .query_write_status(QueryWriteStatusRequest {
            resource_name: upload_name(&digest_of(b"nothing here")),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(!status.complete);
    assert_eq!(status.committed_size, 0);
}

#[tokio::test]
async fn bytestream_write_checks_the_digest() {
    let node = Node::open().await;
    let claimed = Digest {
        hash: digest_of(b"expected").hash,
        size_bytes: 8,
    };
    let err = node
        .bytestream()
        .write(stream::iter(write_requests(
            &upload_name(&claimed),
            b"impostor",
            3,
        )))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    let mismatch = info(&err);
    assert_eq!(mismatch.reason, "DIGEST_MISMATCH");
    assert_eq!(mismatch.metadata["key"], claimed.hash);
    assert_eq!(
        read(&node, &claimed, 0, 0).await.unwrap_err().code(),
        Code::NotFound
    );
}

#[tokio::test]
async fn bytestream_write_checks_offsets_and_length() {
    let node = Node::open().await;
    let data = b"0123456789";
    let digest = digest_of(data);
    // A gap in the offsets.
    let mut requests = write_requests(&upload_name(&digest), data, 4);
    requests[1].write_offset += 1;
    let err = node
        .bytestream()
        .write(stream::iter(requests))
        .await
        .unwrap_err();
    assert_eq!(reason(&err), "INVALID_WRITE");
    // Ends early.
    let mut requests = write_requests(&upload_name(&digest), data, 4);
    requests.pop();
    requests.last_mut().unwrap().finish_write = true;
    let err = node
        .bytestream()
        .write(stream::iter(requests))
        .await
        .unwrap_err();
    assert_eq!(reason(&err), "INVALID_WRITE");
    // More than the digest says.
    let mut requests = write_requests(&upload_name(&digest), data, 4);
    let last = requests.last_mut().unwrap();
    last.data = Bytes::from_static(b"89X");
    let err = node
        .bytestream()
        .write(stream::iter(requests))
        .await
        .unwrap_err();
    assert_eq!(reason(&err), "INVALID_WRITE");
    // A malformed resource name.
    let err = node
        .bytestream()
        .write(stream::iter(write_requests("blobs/abc/10", data, 4)))
        .await
        .unwrap_err();
    assert_eq!(reason(&err), "INVALID_RESOURCE_NAME");
    assert_eq!(
        read(&node, &digest, 0, 0).await.unwrap_err().code(),
        Code::NotFound
    );
}

#[tokio::test]
async fn oversized_blobs_are_refused_without_retry() {
    let node = Node::open().await;
    let data = vec![1_u8; usize::try_from(LIMIT).unwrap() + 1];
    let digest = digest_of(&data);
    let err = node
        .bytestream()
        .write(stream::iter(write_requests(
            &upload_name(&digest),
            &data,
            256 << 10,
        )))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    let too_large = info(&err);
    assert_eq!(too_large.reason, "ENTRY_TOO_LARGE");
    assert_eq!(too_large.metadata["maxEntryBytes"], LIMIT.to_string());
}

fn result_with(outputs: &[&[u8]]) -> ActionResult {
    ActionResult {
        output_files: outputs
            .iter()
            .enumerate()
            .map(|(i, o)| OutputFile {
                path: format!("out/{i}"),
                digest: Some(digest_of(o)),
                ..Default::default()
            })
            .collect(),
        exit_code: 0,
        stdout_digest: Some(digest_of(b"")),
        ..Default::default()
    }
}

#[tokio::test]
async fn action_results_round_trip_and_need_their_outputs() {
    let node = Node::open().await;
    let action = digest_of(b"action");
    let result = result_with(&[b"output one", b"output two"]);
    let err = node
        .ac()
        .get_action_result(GetActionResultRequest {
            action_digest: Some(action.clone()),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);

    let stored = node
        .ac()
        .update_action_result(UpdateActionResultRequest {
            action_digest: Some(action.clone()),
            action_result: Some(result.clone()),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(stored, result);

    // The outputs are not in the CAS yet, so the result is a miss.
    let get = || {
        let mut client = node.ac();
        let action = action.clone();
        async move {
            client
                .get_action_result(GetActionResultRequest {
                    action_digest: Some(action),
                    ..Default::default()
                })
                .await
        }
    };
    assert_eq!(get().await.unwrap_err().code(), Code::NotFound);
    upload(&node, &[b"output one"]).await;
    assert_eq!(get().await.unwrap_err().code(), Code::NotFound);
    upload(&node, &[b"output two"]).await;
    assert_eq!(get().await.unwrap().into_inner(), result);

    // A store replaces the entry.
    let replaced = result_with(&[b"output one"]);
    node.ac()
        .update_action_result(UpdateActionResultRequest {
            action_digest: Some(action.clone()),
            action_result: Some(replaced.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(get().await.unwrap().into_inner(), replaced);
}

#[tokio::test]
async fn shares_entries_with_the_bazel_http_cache() {
    let node = Node::open().await;
    let http = router_with(
        node.cache.clone(),
        None,
        Arc::new(Authenticator::open()),
        Protocols {
            bazel: true,
            ..Protocols::default()
        },
    );
    // Written over HTTP, read over gRPC.
    let blob = b"shared between protocols";
    let digest = digest_of(blob);
    let response = http
        .clone()
        .oneshot(
            HttpRequest::put(format!("/bazel/cas/{}", digest.hash))
                .body(Body::from(&blob[..]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(read(&node, &digest, 0, 0).await.unwrap(), blob);

    // Written over gRPC, read over HTTP.
    let action = digest_of(b"shared action");
    let result = result_with(&[blob]);
    node.ac()
        .update_action_result(UpdateActionResultRequest {
            action_digest: Some(action.clone()),
            action_result: Some(result.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    let response = http
        .oneshot(
            HttpRequest::get(format!("/bazel/ac/{}", action.hash))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(ActionResult::decode(body).unwrap(), result);
}

fn with_auth<T>(message: T, value: &str) -> Request<T> {
    let mut request = Request::new(message);
    request
        .metadata_mut()
        .insert("authorization", MetadataValue::try_from(value).unwrap());
    request
}

#[tokio::test]
async fn calls_need_the_cache_scopes() {
    let auth = Authenticator::new(
        Scopes::default(),
        [
            (
                "ci".to_owned(),
                vec![hash_token("ci-token")],
                [Scope::CacheRead, Scope::CacheWrite].into_iter().collect(),
            ),
            (
                "dev".to_owned(),
                vec![hash_token("dev-token")],
                [Scope::CacheRead].into_iter().collect(),
            ),
        ],
        false,
    );
    let node = Node::start(auth).await;
    let find = || FindMissingBlobsRequest {
        blob_digests: vec![digest_of(b"x")],
        ..Default::default()
    };

    let err = node.cas().find_missing_blobs(find()).await.unwrap_err();
    assert_eq!(err.code(), Code::Unauthenticated);
    assert_eq!(reason(&err), "CREDENTIALS_MISSING");
    let err = node
        .cas()
        .find_missing_blobs(with_auth(find(), "Bearer wrong"))
        .await
        .unwrap_err();
    assert_eq!(reason(&err), "CREDENTIALS_INVALID");

    node.cas()
        .find_missing_blobs(with_auth(find(), "Bearer dev-token"))
        .await
        .unwrap();
    let update = BatchUpdateBlobsRequest {
        requests: vec![batch_update_blobs_request::Request {
            digest: Some(digest_of(b"x")),
            data: Bytes::from_static(b"x"),
            compressor: 0,
        }],
        ..Default::default()
    };
    let err = node
        .cas()
        .batch_update_blobs(with_auth(update.clone(), "Bearer dev-token"))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::PermissionDenied);
    let scope = info(&err);
    assert_eq!(scope.reason, "SCOPE_MISSING");
    assert_eq!(scope.metadata["requiredScope"], "cache:write");

    let basic = format!(
        "Basic {}",
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "ci:ci-token")
    );
    node.cas()
        .batch_update_blobs(with_auth(update, &basic))
        .await
        .unwrap();
    let missing = node
        .cas()
        .find_missing_blobs(with_auth(find(), "Bearer ci-token"))
        .await
        .unwrap()
        .into_inner()
        .missing_blob_digests;
    assert!(missing.is_empty());
}

#[tokio::test]
async fn auth_works_over_ipv6() {
    if std::net::TcpListener::bind("[::1]:0").is_err() {
        eprintln!("skipped: this host has no IPv6");
        return;
    }
    let auth = Authenticator::new(
        Scopes::default(),
        [(
            "ci".to_owned(),
            vec![hash_token("ci-token")],
            [Scope::CacheRead, Scope::CacheWrite].into_iter().collect(),
        )],
        false,
    );
    // The default listen address, reached over IPv6 loopback.
    let listener = mammoth_cache::server::bind("[::]:0".parse().unwrap()).unwrap();
    let port = listener.local_addr().unwrap().port();
    let node = Node::serve(
        auth,
        listener,
        SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], port)),
    )
    .await;
    let update = BatchUpdateBlobsRequest {
        requests: vec![batch_update_blobs_request::Request {
            digest: Some(digest_of(b"v6")),
            data: Bytes::from_static(b"v6"),
            compressor: 0,
        }],
        ..Default::default()
    };
    let err = node
        .cas()
        .batch_update_blobs(update.clone())
        .await
        .unwrap_err();
    assert_eq!(reason(&err), "CREDENTIALS_MISSING");
    let basic = format!(
        "Basic {}",
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "ci:ci-token")
    );
    node.cas()
        .batch_update_blobs(with_auth(update, &basic))
        .await
        .unwrap();
    let find = FindMissingBlobsRequest {
        blob_digests: vec![digest_of(b"v6")],
        ..Default::default()
    };
    let missing = node
        .cas()
        .find_missing_blobs(with_auth(find, "Bearer ci-token"))
        .await
        .unwrap()
        .into_inner()
        .missing_blob_digests;
    assert!(missing.is_empty());
}

#[tokio::test]
async fn concurrent_uploads_of_one_blob_all_succeed() {
    let node = Node::open().await;
    let data: Vec<u8> = (0..100_000_u32).map(|i| (i % 7) as u8).collect();
    let digest = digest_of(&data);
    let calls = (0..8).map(|_| {
        let mut client = node.bytestream();
        let requests = write_requests(&upload_name(&digest), &data, 16 << 10);
        async move { client.write(stream::iter(requests)).await }
    });
    let results: Vec<_> = stream::iter(calls).buffer_unordered(8).collect().await;
    for result in results {
        assert_eq!(
            result.unwrap().into_inner().committed_size,
            digest.size_bytes
        );
    }
    assert_eq!(read(&node, &digest, 0, 0).await.unwrap(), data);
}

#[tokio::test]
async fn stops_on_the_signal() {
    let mut node = Node::open().await;
    node.stop.take().unwrap().send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), &mut node.server)
        .await
        .expect("the server stops")
        .unwrap()
        .unwrap();
}

mod properties {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        /// Any blob, uploaded in any chunk size, reads back whole and in any window, over both
        /// ByteStream and the batch call.
        #[test]
        fn uploads_read_back(
            data in proptest::collection::vec(any::<u8>(), 1..200_000),
            chunk in 1_usize..70_000,
            start in any::<prop::sample::Index>(),
            len in any::<prop::sample::Index>(),
        ) {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async {
                let node = Node::open().await;
                let digest = digest_of(&data);
                let committed = node
                    .bytestream()
                    .write(stream::iter(write_requests(&upload_name(&digest), &data, chunk)))
                    .await
                    .unwrap()
                    .into_inner()
                    .committed_size;
                assert_eq!(committed, digest.size_bytes);
                assert_eq!(read(&node, &digest, 0, 0).await.unwrap(), data);
                let offset = start.index(data.len() + 1);
                let len = len.index(data.len() - offset + 1);
                let window = read(
                    &node,
                    &digest,
                    i64::try_from(offset).unwrap(),
                    i64::try_from(len).unwrap(),
                )
                .await
                .unwrap();
                // A limit of 0 means "to the end".
                let end = if len == 0 { data.len() } else { offset + len };
                assert_eq!(window, &data[offset..end]);
                let batch = node
                    .cas()
                    .batch_read_blobs(BatchReadBlobsRequest {
                        digests: vec![digest],
                        ..Default::default()
                    })
                    .await
                    .unwrap()
                    .into_inner()
                    .responses;
                assert_eq!(batch[0].data, data);
            });
        }

        /// A blob whose bytes are changed anywhere is refused and not stored.
        #[test]
        fn corrupted_uploads_are_refused(
            data in proptest::collection::vec(any::<u8>(), 1..50_000),
            flip in any::<prop::sample::Index>(),
            chunk in 1_usize..20_000,
        ) {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async {
                let node = Node::open().await;
                let digest = digest_of(&data);
                let mut corrupted = data.clone();
                corrupted[flip.index(data.len())] ^= 0x01;
                let err = node
                    .bytestream()
                    .write(stream::iter(write_requests(&upload_name(&digest), &corrupted, chunk)))
                    .await
                    .unwrap_err();
                assert_eq!(reason(&err), "DIGEST_MISMATCH");
                assert_eq!(read(&node, &digest, 0, 0).await.unwrap_err().code(), Code::NotFound);
            });
        }
    }
}

/// Over `[tls]`: the gRPC port negotiates HTTP/2 with ALPN, using the node's certificate.
#[tokio::test]
async fn serves_grpcs_with_the_node_certificate() {
    use mammoth_cache::{server::TlsSettings, tls::Certificates};
    use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
    use tonic::transport::{Certificate, ClientTlsConfig};

    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);
    let leaf_key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["localhost".into()])
        .unwrap()
        .signed_by(&leaf_key, &issuer)
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("tls.crt"), leaf.pem()).unwrap();
    std::fs::write(dir.path().join("tls.key"), leaf_key.serialize_pem()).unwrap();
    let certificates = Arc::new(
        Certificates::load(dir.path().join("tls.crt"), dir.path().join("tls.key")).unwrap(),
    );
    let tls = TlsSettings {
        config: certificates.server_config().unwrap(),
        handshake_timeout: Duration::from_secs(5),
    };

    let cache = Arc::new(
        Cache::builder()
            .bounded_tier(Arc::new(MemoryStore::new(1 << 20)))
            .build(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let settings = settings();
    let routes = reapi::routes(cache, Arc::new(Authenticator::open()), settings);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(reapi::serve(listener, Some(tls), routes, settings, async {
        let _ = stopped.await;
    }));

    let channel = Endpoint::from_shared(format!("https://localhost:{port}"))
        .unwrap()
        .tls_config(
            ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(ca_cert.pem()))
                .domain_name("localhost"),
        )
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut cas = ContentAddressableStorageClient::new(channel);
    let blob: &[u8] = b"over tls";
    cas.batch_update_blobs(BatchUpdateBlobsRequest {
        requests: vec![batch_update_blobs_request::Request {
            digest: Some(digest_of(blob)),
            data: Bytes::from_static(b"over tls"),
            compressor: 0,
        }],
        ..Default::default()
    })
    .await
    .unwrap();
    let missing = cas
        .find_missing_blobs(FindMissingBlobsRequest {
            blob_digests: vec![digest_of(blob)],
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner()
        .missing_blob_digests;
    assert!(missing.is_empty());

    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn find_missing_blobs_is_capped() {
    let node = Node::open().await;
    let digests = |n: usize| -> Vec<Digest> {
        (0..n)
            .map(|i| Digest {
                hash: format!("{i:064x}"),
                size_bytes: 1,
            })
            .collect()
    };
    let find = |digests| {
        let mut cas = node.cas();
        async move {
            cas.find_missing_blobs(FindMissingBlobsRequest {
                blob_digests: digests,
                ..Default::default()
            })
            .await
        }
    };
    let missing = find(digests(10_000)).await.unwrap().into_inner();
    assert_eq!(missing.missing_blob_digests.len(), 10_000);
    let err = find(digests(10_001)).await.unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    assert_eq!(reason(&err), "TOO_MANY_DIGESTS");
}

/// Thousands of small blobs: a batch that fits the advertised data limit is accepted whole,
/// and a read whose response could pass a client's 4 MiB message limit is refused up front
/// rather than failing as it is encoded or decoded.
#[tokio::test]
async fn batches_of_many_small_blobs() {
    let node = Node::open().await;
    let blobs: Vec<Vec<u8>> = (0..20_000_u32)
        .map(|i| format!("small blob {i:08} {}", "x".repeat(40)).into_bytes())
        .collect();
    let refs: Vec<&[u8]> = blobs.iter().map(Vec::as_slice).collect();
    // About 1.2 MB of data in 20,000 entries: well past 4 MiB with framing.
    upload(&node, &refs).await;

    let read = |n: usize| {
        let mut cas = node.cas();
        let digests = refs[..n].iter().map(|b| digest_of(b)).collect();
        async move {
            cas.batch_read_blobs(BatchReadBlobsRequest {
                digests,
                ..Default::default()
            })
            .await
        }
    };
    let err = read(20_000).await.unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    assert_eq!(reason(&err), "BATCH_TOO_LARGE");
    // A default tonic client, with its 4 MiB limit, takes the largest batch that is allowed.
    let fits = usize::try_from((4 << 20) / (64 + 256)).unwrap();
    let response = read(fits).await.unwrap().into_inner();
    assert_eq!(response.responses.len(), fits);
    assert!(
        response
            .responses
            .iter()
            .all(|r| r.status.as_ref().unwrap().code == 0)
    );
}

#[tokio::test]
async fn batch_sizes_from_the_client_cannot_overflow() {
    let node = Node::open().await;
    let huge = |c: char| Digest {
        hash: c.to_string().repeat(64),
        size_bytes: i64::MAX,
    };
    let err = node
        .cas()
        .batch_read_blobs(BatchReadBlobsRequest {
            digests: vec![huge('a'), huge('b'), huge('c')],
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    assert_eq!(reason(&err), "BATCH_TOO_LARGE");
}

/// An action result with a directory output is a hit only while every file in its tree is
/// present, and a miss is counted as one.
#[tokio::test]
async fn action_results_need_the_files_in_their_trees() {
    let node = Node::open().await;
    let file = |data: &[u8]| FileNode {
        name: "f".to_owned(),
        digest: Some(digest_of(data)),
        ..Default::default()
    };
    let tree = Tree {
        root: Some(Directory {
            files: vec![file(b"top file")],
            ..Default::default()
        }),
        children: vec![Directory {
            files: vec![file(b"nested file")],
            ..Default::default()
        }],
    }
    .encode_to_vec();
    let result = ActionResult {
        output_directories: vec![OutputDirectory {
            path: "out".to_owned(),
            tree_digest: Some(digest_of(&tree)),
            ..Default::default()
        }],
        ..Default::default()
    };
    let action = digest_of(b"tree action");
    node.ac()
        .update_action_result(UpdateActionResultRequest {
            action_digest: Some(action.clone()),
            action_result: Some(result.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    let get = || {
        let mut client = node.ac();
        let action = action.clone();
        async move {
            client
                .get_action_result(GetActionResultRequest {
                    action_digest: Some(action),
                    ..Default::default()
                })
                .await
        }
    };

    // The tree is there but a file inside it is not, as after an eviction.
    upload(&node, &[&tree, b"top file"]).await;
    let before = node.cache.status();
    assert_eq!(get().await.unwrap_err().code(), Code::NotFound);
    let after = node.cache.status();
    assert_eq!(after.hits - before.hits, 0, "a stale result is not a hit");
    assert_eq!(after.misses - before.misses, 1);

    upload(&node, &[b"nested file"]).await;
    assert_eq!(get().await.unwrap().into_inner(), result);
    assert_eq!(node.cache.status().hits - after.hits, 1);
}

/// A stored action result too large to send is a miss, refused before it is read.
#[tokio::test]
async fn oversized_action_results_are_misses() {
    let node = Node::with(16 << 20, settings()).await;
    let action = digest_of(b"huge result");
    let key = CacheKey::bazel(Namespace::BazelAc, &action.hash).unwrap();
    let data = Bytes::from(vec![0_u8; 5 << 20]);
    node.cache
        .put(
            &key,
            Some(5 << 20),
            futures_util::stream::once(async move { Ok(data) }).boxed(),
        )
        .await
        .unwrap();
    let err = node
        .ac()
        .get_action_result(GetActionResultRequest {
            action_digest: Some(action),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
}

/// A download whose client stops taking data ends with `DEADLINE_EXCEEDED` after `send_timeout`,
/// rather than holding the entry open.
#[tokio::test]
async fn a_stalled_read_is_ended() {
    let node = Node::with(
        16 << 20,
        ServerSettings {
            send_timeout: Duration::from_millis(300),
            ..settings()
        },
    )
    .await;
    // More than the client's HTTP/2 windows and the server's buffers hold.
    let data = vec![3_u8; 12 << 20];
    let digest = digest_of(&data);
    node.bytestream()
        .write(futures_util::stream::iter(write_requests(
            &upload_name(&digest),
            &data,
            1 << 20,
        )))
        .await
        .unwrap();

    let mut stream = node
        .bytestream()
        .read(ReadRequest {
            resource_name: format!("instance/blobs/{}/{}", digest.hash, digest.size_bytes),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let mut received = stream.message().await.unwrap().unwrap().data.len();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let err = loop {
        match stream.message().await {
            Ok(Some(message)) => received += message.data.len(),
            Ok(None) => panic!("the stalled read finished"),
            Err(err) => break err,
        }
    };
    assert!(received < data.len());
    assert_eq!(err.code(), Code::DeadlineExceeded);
    assert_eq!(reason(&err), "DOWNLOAD_STALLED");
}

/// A unary call whose request body stops arriving fails after `body_read_timeout`, freeing its
/// place among the buffered calls.
#[tokio::test]
async fn a_stalled_unary_request_fails() {
    let node = Node::with(
        LIMIT,
        ServerSettings {
            body_read_timeout: Duration::from_millis(300),
            ..settings()
        },
    )
    .await;
    let mut grpc = tonic::client::Grpc::new(node.channel.clone());
    grpc.ready().await.unwrap();
    let path = tonic::codegen::http::uri::PathAndQuery::from_static(
        "/build.bazel.remote.execution.v2.ContentAddressableStorage/BatchUpdateBlobs",
    );
    // Headers only; the message never comes.
    let call = grpc.client_streaming::<_, BatchUpdateBlobsRequest, BatchUpdateBlobsResponse, _>(
        Request::new(futures_util::stream::pending()),
        path,
        tonic_prost::ProstCodec::default(),
    );
    let result = tokio::time::timeout(Duration::from_secs(3), call)
        .await
        .expect("the stalled request was never failed");
    assert!(result.is_err());
}

/// A batch upload over `max_batch_total_size_bytes` is refused whole, as a batch read is, and one
/// past the message limit gets `RESOURCE_EXHAUSTED`, never `OUT_OF_RANGE`.
#[tokio::test]
async fn oversized_batch_uploads_are_refused_whole() {
    let node = Node::with(16 << 20, settings()).await;
    let update = |blobs: Vec<Vec<u8>>| {
        let mut cas = node.cas();
        async move {
            cas.batch_update_blobs(BatchUpdateBlobsRequest {
                requests: blobs
                    .iter()
                    .map(|b| batch_update_blobs_request::Request {
                        digest: Some(digest_of(b)),
                        data: Bytes::copy_from_slice(b),
                        compressor: 0,
                    })
                    .collect(),
                ..Default::default()
            })
            .await
        }
    };
    let half = (2 << 20) + (64 << 10);
    let blobs = vec![vec![1_u8; half], vec![2_u8; half]];
    let err = update(blobs.clone()).await.unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    assert_eq!(reason(&err), "BATCH_TOO_LARGE");
    let missing = node
        .cas()
        .find_missing_blobs(FindMissingBlobsRequest {
            blob_digests: blobs.iter().map(|b| digest_of(b)).collect(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        missing.missing_blob_digests.len(),
        2,
        "a refused batch stored a blob"
    );

    let err = update(vec![vec![3_u8; 9 << 20]]).await.unwrap_err();
    assert_eq!(err.code(), Code::ResourceExhausted);
    assert_eq!(reason(&err), "MESSAGE_TOO_LARGE");
}
