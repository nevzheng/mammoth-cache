//! The accept loop when the process is out of file descriptors. Its own test binary, because it
//! lowers the process's descriptor limit and installs a log subscriber.
#![cfg(unix)]
// Helpers panic on unexpected values, like the tests that call them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    io::Write,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use mammoth_cache::{
    cache::Cache,
    router,
    server::{self, ServerSettings},
    store::memory::MemoryStore,
};
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

/// Log lines, shared with the subscriber.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Logs {
    fn count(&self, needle: &str) -> usize {
        let logs = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&logs).matches(needle).count()
    }
}

impl Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn open_descriptors() -> u64 {
    std::fs::read_dir("/dev/fd").unwrap().count() as u64
}

#[tokio::test]
async fn running_out_of_descriptors_pauses_the_accept_loop() {
    let logs = Logs::default();
    let writer = logs.clone();
    tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .init();

    let cache = Arc::new(
        Cache::builder()
            .bounded_tier(Arc::new(MemoryStore::new(1 << 20)))
            .build(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let server = tokio::spawn(server::serve(
        listener,
        router(cache.clone(), None),
        cache,
        ServerSettings {
            shutdown_delay: Duration::ZERO,
            shutdown_timeout: Duration::from_secs(1),
            ..ServerSettings::default()
        },
        async {
            let _ = stopped.await;
        },
    ));

    // Leave a few descriptors, then connect until none are left. The server's accepts then fail
    // with EMFILE while connections wait in the backlog.
    let original = getrlimit(Resource::Nofile);
    let tight = open_descriptors() + 8;
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(tight),
            maximum: original.maximum,
        },
    )
    .unwrap();
    let mut clients = Vec::new();
    while let Ok(stream) = TcpStream::connect(addr).await {
        clients.push(stream);
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let while_failing = logs.count("cannot accept connections");

    drop(clients);
    setrlimit(Resource::Nofile, original).unwrap();
    // The node serves again.
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET /ping HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
        .await
        .expect("no response after descriptors were freed")
        .unwrap();
    assert!(response.starts_with(b"HTTP/1.1 200"));

    // One warning when accepts start failing, not one per spin of the loop.
    assert_eq!(
        while_failing, 1,
        "accept errors were logged {while_failing} times"
    );
    assert_eq!(logs.count("accepting connections again"), 1);
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}
