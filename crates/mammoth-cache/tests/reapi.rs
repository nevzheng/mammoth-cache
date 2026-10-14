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
    reapi::{
        self, GrpcSettings,
        proto::{
            build::bazel::remote::execution::v2::{
                ActionResult, BatchReadBlobsRequest, BatchUpdateBlobsRequest, Digest,
                ExecuteRequest, FindMissingBlobsRequest, GetActionResultRequest,
                GetCapabilitiesRequest, GetTreeRequest, OutputFile, UpdateActionResultRequest,
                action_cache_client::ActionCacheClient, batch_update_blobs_request,
                capabilities_client::CapabilitiesClient,
                content_addressable_storage_client::ContentAddressableStorageClient,
                digest_function, execution_client::ExecutionClient,
            },
            google::{
                bytestream::{
                    QueryWriteStatusRequest, ReadRequest, WriteRequest,
                    byte_stream_client::ByteStreamClient,
                },
                rpc,
            },
        },
    },
    router_with,
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
        let cache = Arc::new(
            Cache::builder()
                .bounded_tier(Arc::new(
                    MemoryStore::<FoyerBackend>::with_config(MemoryConfig {
                        max_entry_bytes: LIMIT,
                        ..MemoryConfig::new(64 << 20)
                    })
                    .unwrap(),
                ))
                .max_entry_size(LIMIT)
                .build(),
        );
        let routes = reapi::routes(cache.clone(), Arc::new(auth), Duration::from_secs(5));
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let settings = GrpcSettings {
            shutdown_delay: Duration::ZERO,
            shutdown_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(5),
        };
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

fn reason(status: &Status) -> String {
    let details = rpc::Status::decode(status.details()).unwrap();
    let info = rpc::ErrorInfo::decode(details.details[0].value.as_slice()).unwrap();
    assert_eq!(info.domain, "mammoth-cache.dev");
    info.reason
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
    assert_eq!(cache.max_batch_total_size_bytes, 4 << 20);
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
    assert_eq!(reason(&err), "ENTRY_NOT_FOUND");
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
    assert_eq!(reason(&err), "DIGEST_MISMATCH");
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
    assert_eq!(reason(&err), "ENTRY_TOO_LARGE");
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
    assert_eq!(reason(&err), "SCOPE_MISSING");

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
    let routes = reapi::routes(
        cache,
        Arc::new(Authenticator::open()),
        Duration::from_secs(5),
    );
    let settings = GrpcSettings {
        shutdown_delay: Duration::ZERO,
        shutdown_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(5),
    };
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
