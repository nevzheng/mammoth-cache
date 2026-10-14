//! A whole node: the HTTP and gRPC servers together, as `main` runs them, through the shutdown
//! drain and the bucket flush that follows it (design docs 0013 and 0016).

// Helpers panic on unexpected values, like the tests that call them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    io,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use futures_util::stream;
use mammoth_cache::{
    auth::Authenticator,
    cache::{BucketWrite, Cache},
    key::{CacheKey, Namespace},
    reapi::{
        self,
        proto::google::bytestream::{WriteRequest, byte_stream_client::ByteStreamClient},
    },
    router,
    server::{self, ServerSettings},
    store::{
        BlobStore,
        bucket::{BucketConfig, BucketStore},
        disk::{DiskConfig, DiskStore},
        memory::MemoryStore,
    },
};
use sha2::{Digest as _, Sha256};
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
    sync::{mpsc, watch},
    task::JoinHandle,
    time::timeout,
};
use tonic::transport::Endpoint;

struct Node {
    grpc: SocketAddr,
    stop: watch::Sender<bool>,
    run: JoinHandle<io::Result<()>>,
}

/// Starts both servers over `cache` and runs them as `main` does.
async fn start(cache: Cache, settings: ServerSettings) -> Node {
    let cache = Arc::new(cache);
    let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_addr = grpc.local_addr().unwrap();
    let (stop, _) = watch::channel(false);
    let signal = || {
        let mut stopped = stop.subscribe();
        async move {
            let _ = stopped.wait_for(|stop| *stop).await;
        }
    };
    let routes = reapi::routes(cache.clone(), Arc::new(Authenticator::open()), settings);
    let servers = vec![
        (
            "gRPC",
            tokio::spawn(reapi::serve(grpc, None, routes, settings, signal())),
        ),
        (
            "HTTP",
            tokio::spawn(server::serve(
                http,
                router(cache.clone(), None),
                cache.clone(),
                settings,
                signal(),
            )),
        ),
    ];
    let run = {
        let stop = stop.clone();
        tokio::spawn(async move {
            server::run_node(&cache, servers, &stop, settings.shutdown_timeout).await
        })
    };
    Node {
        grpc: grpc_addr,
        stop,
        run,
    }
}

fn settings() -> ServerSettings {
    ServerSettings {
        shutdown_delay: Duration::from_millis(200),
        shutdown_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(30),
        body_read_timeout: Duration::from_secs(5),
        send_timeout: Duration::from_secs(5),
    }
}

/// A gRPC write still streaming after the HTTP server has drained reaches the bucket: the flush
/// waits for both servers.
#[tokio::test]
async fn a_grpc_write_finishing_after_the_http_drain_reaches_the_bucket() {
    let dir = tempfile::tempdir().unwrap();
    let disk = DiskStore::open(DiskConfig {
        path: dir.path().to_path_buf(),
        capacity_bytes: 1 << 20,
        max_entry_age: None,
        free_space_buffer: 0,
    })
    .await
    .unwrap();
    let bucket = Arc::new(BucketStore::open(&BucketConfig::new("memory:///")).unwrap());
    // Background uploads, which read the entry back from disk.
    let cache = Cache::builder()
        .bounded_tier(Arc::new(disk))
        .bucket_tier(bucket.clone(), BucketWrite::Async { queue: 16 })
        .build();
    let node = start(cache, settings()).await;

    let data = vec![7_u8; 4096];
    let hash = hex(&Sha256::digest(&data));
    let resource = format!(
        "uploads/7d3f1a2e-1111-4c2b-9a8e-123456789abc/blobs/{hash}/{}",
        data.len()
    );
    let channel = Endpoint::from_shared(format!("http://{}", node.grpc))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let (tx, rx) = mpsc::channel(4);
    let write = tokio::spawn(async move {
        ByteStreamClient::new(channel)
            .write(stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|message| (message, rx))
            }))
            .await
    });
    let (head, tail) = data.split_at(1024);
    tx.send(WriteRequest {
        resource_name: resource.clone(),
        write_offset: 0,
        finish_write: false,
        data: head.to_vec().into(),
    })
    .await
    .unwrap();

    // The drain starts with the write half sent, and the HTTP server, idle, drains at once.
    node.stop.send_replace(true);
    tokio::time::sleep(settings().shutdown_delay * 3).await;
    tx.send(WriteRequest {
        resource_name: resource,
        write_offset: 1024,
        finish_write: true,
        data: tail.to_vec().into(),
    })
    .await
    .unwrap();
    let committed = write.await.unwrap().unwrap().into_inner().committed_size;
    assert_eq!(committed, 4096);

    node.run.await.unwrap().unwrap();
    let key = CacheKey::bazel(Namespace::BazelCas, &hash).unwrap();
    assert_eq!(bucket.head(&key).await.unwrap(), Some(4096));
}

/// A connection to the gRPC port that never sends anything is closed after `idle_timeout`.
#[tokio::test]
async fn a_silent_grpc_connection_is_closed() {
    let cache = Cache::builder()
        .bounded_tier(Arc::new(MemoryStore::new(1 << 20)))
        .build();
    let node = start(
        cache,
        ServerSettings {
            idle_timeout: Duration::from_millis(300),
            ..settings()
        },
    )
    .await;
    let mut silent = TcpStream::connect(node.grpc).await.unwrap();
    let started = Instant::now();
    let mut buf = vec![0_u8; 1024];
    // The server's HTTP/2 settings arrive first; then the connection ends.
    loop {
        match timeout(Duration::from_secs(5), silent.read(&mut buf))
            .await
            .expect("the silent connection was never closed")
        {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
    assert!(started.elapsed() >= Duration::from_millis(250));
}

/// A connection that has sent nothing does not hold up the drain until `shutdown_timeout`.
#[tokio::test]
async fn a_silent_grpc_connection_does_not_hold_the_drain() {
    let cache = Cache::builder()
        .bounded_tier(Arc::new(MemoryStore::new(1 << 20)))
        .build();
    let node = start(cache, settings()).await;
    let _silent = TcpStream::connect(node.grpc).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let started = Instant::now();
    node.stop.send_replace(true);
    timeout(Duration::from_secs(3), node.run)
        .await
        .expect("the drain waited for a silent connection")
        .unwrap()
        .unwrap();
    assert!(started.elapsed() < settings().shutdown_timeout);
}

/// A server that stops before the shutdown signal stops the node, with an error.
#[tokio::test]
async fn a_server_stopping_early_stops_the_node() {
    let cache = Arc::new(
        Cache::builder()
            .bounded_tier(Arc::new(MemoryStore::new(1 << 20)))
            .build(),
    );
    let (stop, _) = watch::channel(false);
    let mut stopped = stop.subscribe();
    let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = tokio::spawn(server::serve(
        http,
        router(cache.clone(), None),
        cache.clone(),
        settings(),
        async move {
            let _ = stopped.wait_for(|stop| *stop).await;
        },
    ));
    let dead = tokio::spawn(async { Err(io::Error::other("gone")) });
    let servers = vec![("gRPC", dead), ("HTTP", http)];
    let result = timeout(
        Duration::from_secs(3),
        server::run_node(&cache, servers, &stop, settings().shutdown_timeout),
    )
    .await
    .expect("the other server was not stopped");
    assert!(result.is_err());
    assert!(*stop.borrow());
    assert!(cache.is_shutting_down());
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}
