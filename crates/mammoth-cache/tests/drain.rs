//! The server's connection handling over a real socket: the shutdown drain and the timeouts
//! (design doc 0013).

// Helpers panic on unexpected values, like the tests that call them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{net::SocketAddr, sync::Arc, time::Duration};

use mammoth_cache::{
    cache::Cache,
    key::CacheKey,
    router,
    server::{self, ServerSettings},
    store::{
        disk::{DiskConfig, DiskStore},
        memory::MemoryStore,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
    time::timeout,
};

const KEY: &str = "0123456789abcdef";

struct Node {
    addr: SocketAddr,
    cache: Arc<Cache>,
    stop: Option<oneshot::Sender<()>>,
    server: JoinHandle<std::io::Result<()>>,
}

async fn start(settings: ServerSettings) -> Node {
    let cache = Cache::builder()
        .bounded_tier(Arc::new(MemoryStore::new(1 << 20)))
        .build();
    start_with(cache, settings).await
}

async fn start_with(cache: Cache, settings: ServerSettings) -> Node {
    let cache = Arc::new(cache);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel();
    let app = router(cache.clone(), None);
    let server = tokio::spawn(server::serve(
        listener,
        app,
        cache.clone(),
        settings,
        async {
            let _ = stopped.await;
        },
    ));
    Node {
        addr,
        cache,
        stop: Some(stop),
        server,
    }
}

fn fast() -> ServerSettings {
    ServerSettings {
        shutdown_delay: Duration::from_millis(300),
        shutdown_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(5),
        body_read_timeout: Duration::from_secs(5),
        send_timeout: Duration::from_secs(5),
    }
}

/// Reads until the response head is complete and returns it.
async fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let n = timeout(Duration::from_secs(5), stream.read(&mut byte))
            .await
            .expect("timed out waiting for a response")
            .unwrap();
        assert_ne!(n, 0, "connection closed before a response");
        head.push(byte[0]);
    }
    String::from_utf8(head).unwrap()
}

async fn get(addr: SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    timeout(Duration::from_secs(5), stream.read_to_string(&mut response))
        .await
        .unwrap()
        .unwrap();
    response
}

#[tokio::test]
async fn shutdown_fails_readiness_and_finishes_an_upload_in_flight() {
    let mut node = start(fast()).await;
    assert!(get(node.addr, "/readyz").await.starts_with("HTTP/1.1 200"));

    // An upload is half sent when the signal arrives.
    let mut upload = TcpStream::connect(node.addr).await.unwrap();
    let head = format!("PUT /cache/{KEY} HTTP/1.1\r\nHost: test\r\nContent-Length: 10\r\n\r\n");
    upload.write_all(head.as_bytes()).await.unwrap();
    upload.write_all(b"01234").await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    node.stop.take().unwrap().send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // During the delay, new connections are served, and readiness fails.
    let readyz = get(node.addr, "/readyz").await;
    assert!(readyz.starts_with("HTTP/1.1 503"), "{readyz}");
    assert!(readyz.contains("SHUTTING_DOWN"), "{readyz}");
    assert!(get(node.addr, "/livez").await.starts_with("HTTP/1.1 200"));

    // The upload finishes after the listener has closed.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(TcpStream::connect(node.addr).await.is_err());
    upload.write_all(b"56789").await.unwrap();
    let response = read_head(&mut upload).await;
    assert!(response.starts_with("HTTP/1.1 201"), "{response}");
    drop(upload);

    timeout(Duration::from_secs(5), node.server)
        .await
        .expect("the server did not stop")
        .unwrap()
        .unwrap();
    let key = CacheKey::gradle(KEY).unwrap();
    assert!(node.cache.head(&key).await.unwrap().is_some());
}

#[tokio::test]
async fn the_deadline_abandons_a_stalled_upload() {
    let settings = ServerSettings {
        shutdown_delay: Duration::from_millis(100),
        shutdown_timeout: Duration::from_millis(500),
        ..fast()
    };
    let mut node = start(settings).await;
    let mut upload = TcpStream::connect(node.addr).await.unwrap();
    let head = format!("PUT /cache/{KEY} HTTP/1.1\r\nHost: test\r\nContent-Length: 10\r\n\r\n");
    upload.write_all(head.as_bytes()).await.unwrap();
    upload.write_all(b"01234").await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    node.stop.take().unwrap().send(()).unwrap();

    // The client never sends the rest; the server stops at the deadline anyway.
    timeout(Duration::from_secs(2), node.server)
        .await
        .expect("the server waited past its deadline")
        .unwrap()
        .unwrap();
    drop(upload);
}

#[tokio::test]
async fn an_idle_connection_is_closed() {
    let node = start(ServerSettings {
        idle_timeout: Duration::from_millis(300),
        ..fast()
    })
    .await;
    let mut stream = TcpStream::connect(node.addr).await.unwrap();
    let mut buf = [0_u8; 1024];
    let read = timeout(Duration::from_secs(3), stream.read(&mut buf))
        .await
        .expect("the idle connection stayed open");
    // Closed (0 bytes or a reset), possibly after a 408 response.
    match read {
        Ok(0) | Err(_) => {}
        Ok(n) => assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 408")),
    }
}

#[tokio::test]
async fn an_upload_that_stops_sending_fails_and_stores_nothing() {
    let node = start(ServerSettings {
        body_read_timeout: Duration::from_millis(300),
        ..fast()
    })
    .await;
    let mut upload = TcpStream::connect(node.addr).await.unwrap();
    let head = format!("PUT /cache/{KEY} HTTP/1.1\r\nHost: test\r\nContent-Length: 10\r\n\r\n");
    upload.write_all(head.as_bytes()).await.unwrap();
    upload.write_all(b"01234").await.unwrap();

    // Within a second, the server answers or closes the connection rather than wait forever.
    let mut buf = [0_u8; 1024];
    timeout(Duration::from_secs(2), upload.read(&mut buf))
        .await
        .expect("the stalled upload was never ended")
        .ok();
    let key = CacheKey::gradle(KEY).unwrap();
    assert!(node.cache.head(&key).await.unwrap().is_none());
}

#[tokio::test]
async fn readiness_reports_the_disk_tier() {
    // One directory per node: a directory serves one store at a time.
    let (dir, other) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let disk = |dir: &tempfile::TempDir, free_space_buffer| DiskConfig {
        path: dir.path().to_path_buf(),
        capacity_bytes: 1 << 20,
        max_entry_age: None,
        free_space_buffer,
    };

    let healthy = DiskStore::open(disk(&dir, 0)).await.unwrap();
    let node = start_with(
        Cache::builder().bounded_tier(Arc::new(healthy)).build(),
        fast(),
    )
    .await;
    let verbose = get(node.addr, "/readyz?verbose").await;
    for line in [
        "[+]config ok",
        "[+]shutting-down ok",
        "[+]storage-writable ok",
        "[+]storage-free-space ok",
        "readyz check passed",
    ] {
        assert!(verbose.contains(line), "{verbose}");
    }

    // A buffer larger than any volume.
    let short = DiskStore::open(disk(&other, u64::MAX / 2)).await.unwrap();
    let node = start_with(
        Cache::builder().bounded_tier(Arc::new(short)).build(),
        fast(),
    )
    .await;
    let readyz = get(node.addr, "/readyz").await;
    assert!(readyz.starts_with("HTTP/1.1 503"), "{readyz}");
    for part in [
        "STORAGE_FREE_SPACE_LOW",
        r#""check":"storage-free-space""#,
        "bufferBytes",
    ] {
        assert!(readyz.contains(part), "{readyz}");
    }
}
