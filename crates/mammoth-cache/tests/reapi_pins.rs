//! Pins the gRPC code of every error the REAPI cache answers, as main answers it today. Design
//! doc 0017 rebuilds error handling in steps; each step may change `ErrorInfo` details but must
//! pass this file unchanged, since Bazel and Buck2 read only the code (and retry by it).

// Helpers panic on unexpected values, like the tests that call them.
#![allow(clippy::unwrap_used)]

use std::{fmt::Write, io, sync::Arc, time::Duration};

use bytes::Bytes;
use futures_util::stream;
use mammoth_cache::{
    auth::{Authenticator, Scope, Scopes, hash_token},
    cache::Cache,
    key::CacheKey,
    reapi::{
        self,
        proto::{
            build::bazel::remote::execution::v2::{
                BatchUpdateBlobsRequest, Digest, ExecuteRequest, FindMissingBlobsRequest,
                GetActionResultRequest, GetTreeRequest, action_cache_client::ActionCacheClient,
                batch_update_blobs_request,
                content_addressable_storage_client::ContentAddressableStorageClient,
                digest_function, execution_client::ExecutionClient,
            },
            google::bytestream::{ReadRequest, WriteRequest, byte_stream_client::ByteStreamClient},
        },
    },
    server::ServerSettings,
    store::{
        Blob, BlobStore, BoundedStore, BoxFuture, ByteStream, PutOutcome, Tier, TierUsage,
        memory::MemoryStore,
    },
};
use sha2::{Digest as _, Sha256};
use tokio::net::TcpListener;
use tonic::{
    Code, Request, Status,
    metadata::MetadataValue,
    transport::{Channel, Endpoint},
};

const LIMIT: u64 = 16;

async fn serve(cache: Cache, auth: Authenticator) -> Channel {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let settings = ServerSettings {
        shutdown_delay: Duration::ZERO,
        shutdown_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(5),
        ..ServerSettings::default()
    };
    let routes = reapi::routes(Arc::new(cache), Arc::new(auth), settings);
    tokio::spawn(reapi::serve(
        listener,
        None,
        routes,
        settings,
        std::future::pending::<()>(),
    ));
    Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap()
}

fn memory() -> Cache {
    Cache::builder()
        .bounded_tier(Arc::new(MemoryStore::new(1 << 20)))
        .max_entry_size(LIMIT)
        .build()
}

/// A local tier whose stores fail with an `io::Error` of the given kind.
#[derive(Debug)]
struct Failing(io::ErrorKind);

impl BlobStore for Failing {
    fn tier(&self) -> Tier {
        Tier::Memory
    }

    fn get<'a>(&'a self, _key: &'a CacheKey) -> BoxFuture<'a, io::Result<Option<Blob>>> {
        Box::pin(async { Ok(None) })
    }

    fn head<'a>(&'a self, _key: &'a CacheKey) -> BoxFuture<'a, io::Result<Option<u64>>> {
        Box::pin(async { Ok(None) })
    }

    fn put<'a>(
        &'a self,
        _key: &'a CacheKey,
        _size_hint: Option<u64>,
        _body: ByteStream,
    ) -> BoxFuture<'a, io::Result<PutOutcome>> {
        let kind = self.0;
        Box::pin(async move { Err(io::Error::new(kind, "the test tier fails")) })
    }
}

impl BoundedStore for Failing {
    fn usage(&self) -> TierUsage {
        TierUsage::default()
    }
}

fn failing(kind: io::ErrorKind) -> Cache {
    Cache::builder()
        .bounded_tier(Arc::new(Failing(kind)))
        .max_entry_size(LIMIT)
        .build()
}

fn digest_of(data: &[u8]) -> Digest {
    let hash = Sha256::digest(data).iter().fold(String::new(), |mut s, b| {
        write!(s, "{b:02x}").unwrap();
        s
    });
    Digest {
        hash,
        size_bytes: i64::try_from(data.len()).unwrap(),
    }
}

fn blob_name(digest: &Digest) -> String {
    format!("instance/blobs/{}/{}", digest.hash, digest.size_bytes)
}

fn upload_name(digest: &Digest) -> String {
    format!(
        "instance/uploads/7d3f1a2e-1111-4c2b-9a8e-123456789abc/blobs/{}/{}",
        digest.hash, digest.size_bytes
    )
}

async fn read(channel: &Channel, resource: String, offset: i64, limit: i64) -> Status {
    let result = ByteStreamClient::new(channel.clone())
        .read(ReadRequest {
            resource_name: resource,
            read_offset: offset,
            read_limit: limit,
        })
        .await;
    match result {
        Err(status) => status,
        Ok(response) => {
            let mut stream = response.into_inner();
            loop {
                match stream.message().await {
                    Err(status) => return status,
                    Ok(None) => return Status::ok(""),
                    Ok(Some(_)) => {}
                }
            }
        }
    }
}

/// One `Write` call carrying `data` in one message, claiming `digest`.
async fn write(channel: &Channel, digest: &Digest, data: &'static [u8]) -> Code {
    let request = WriteRequest {
        resource_name: upload_name(digest),
        write_offset: 0,
        finish_write: true,
        data: Bytes::from_static(data),
    };
    match ByteStreamClient::new(channel.clone())
        .write(stream::iter([request]))
        .await
    {
        Ok(_) => Code::Ok,
        Err(status) => status.code(),
    }
}

/// The per-blob codes of one `BatchUpdateBlobs` call, or the call's code if it failed.
async fn batch_update(channel: &Channel, blobs: &[(Digest, &'static [u8])]) -> Vec<Code> {
    let request = BatchUpdateBlobsRequest {
        requests: blobs
            .iter()
            .map(|(digest, data)| batch_update_blobs_request::Request {
                digest: Some(digest.clone()),
                data: Bytes::from_static(data),
                compressor: 0,
            })
            .collect(),
        ..Default::default()
    };
    match ContentAddressableStorageClient::new(channel.clone())
        .batch_update_blobs(request)
        .await
    {
        Ok(response) => response
            .into_inner()
            .responses
            .into_iter()
            .map(|r| Code::from(r.status.unwrap().code))
            .collect(),
        Err(status) => vec![status.code()],
    }
}

fn with_auth<T>(message: T, value: &str) -> Request<T> {
    let mut request = Request::new(message);
    request
        .metadata_mut()
        .insert("authorization", MetadataValue::try_from(value).unwrap());
    request
}

#[tokio::test]
async fn cas_and_bytestream_codes() {
    let channel = serve(memory(), Authenticator::open()).await;
    let missing = digest_of(b"missing");
    let over = digest_of(&[b'x'; 32]);

    assert_eq!(
        read(&channel, blob_name(&missing), 0, 0).await.code(),
        Code::NotFound
    );
    assert_eq!(
        read(&channel, "not a resource".to_owned(), 0, 0)
            .await
            .code(),
        Code::InvalidArgument
    );
    assert_eq!(
        read(&channel, blob_name(&missing), 0, -1).await.code(),
        Code::InvalidArgument
    );

    let stored = digest_of(b"stored");
    assert_eq!(
        batch_update(&channel, &[(stored.clone(), b"stored")]).await,
        [Code::Ok]
    );
    assert_eq!(
        read(&channel, blob_name(&stored), 7, 0).await.code(),
        Code::OutOfRange
    );

    let wrong = Digest {
        hash: digest_of(b"expected").hash,
        size_bytes: 8,
    };
    assert_eq!(
        write(&channel, &wrong, b"12345678").await,
        Code::InvalidArgument
    );
    assert_eq!(
        write(&channel, &over, &[b'x'; 32]).await,
        Code::InvalidArgument
    );
    assert_eq!(
        batch_update(&channel, &[(wrong, b"12345678"), (over, &[b'x'; 32])]).await,
        [Code::InvalidArgument, Code::InvalidArgument]
    );

    let err = ContentAddressableStorageClient::new(channel.clone())
        .find_missing_blobs(FindMissingBlobsRequest {
            digest_function: digest_function::Value::Blake3 as i32,
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn action_cache_and_unserved_codes() {
    let channel = serve(memory(), Authenticator::open()).await;
    let err = ActionCacheClient::new(channel.clone())
        .get_action_result(GetActionResultRequest {
            action_digest: Some(digest_of(b"action")),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
    let err = ActionCacheClient::new(channel.clone())
        .get_action_result(GetActionResultRequest::default())
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);

    let err = ExecutionClient::new(channel.clone())
        .execute(ExecuteRequest::default())
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unimplemented);
    let err = ContentAddressableStorageClient::new(channel)
        .get_tree(GetTreeRequest::default())
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unimplemented);
}

#[tokio::test]
async fn auth_codes() {
    let auth = Authenticator::new(
        Scopes::default(),
        [(
            "dev".to_owned(),
            vec![hash_token("dev-token")],
            [Scope::CacheRead].into_iter().collect::<Scopes>(),
        )],
        false,
    );
    let channel = serve(memory(), auth).await;
    let cas = || ContentAddressableStorageClient::new(channel.clone());
    let find = FindMissingBlobsRequest::default;

    assert_eq!(
        cas().find_missing_blobs(find()).await.unwrap_err().code(),
        Code::Unauthenticated
    );
    assert_eq!(
        cas()
            .find_missing_blobs(with_auth(find(), "Bearer wrong"))
            .await
            .unwrap_err()
            .code(),
        Code::Unauthenticated
    );
    assert_eq!(
        cas()
            .batch_update_blobs(with_auth(
                BatchUpdateBlobsRequest::default(),
                "Bearer dev-token"
            ))
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
}

#[tokio::test]
async fn storage_failure_codes() {
    for (kind, want) in [
        (io::ErrorKind::TimedOut, Code::Unavailable),
        (io::ErrorKind::WouldBlock, Code::Unavailable),
        (io::ErrorKind::PermissionDenied, Code::Internal),
        (io::ErrorKind::Other, Code::Internal),
    ] {
        let channel = serve(failing(kind), Authenticator::open()).await;
        let digest = digest_of(b"data");
        assert_eq!(
            batch_update(&channel, &[(digest.clone(), b"data")]).await,
            [want],
            "{kind:?} in a batch"
        );
        assert_eq!(
            write(&channel, &digest, b"data").await,
            want,
            "{kind:?} in a write"
        );
    }
}
