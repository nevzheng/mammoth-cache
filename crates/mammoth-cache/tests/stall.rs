//! A client that stops reading a response: the connection is closed after `send_timeout`, rather
//! than held, with its task and the entry, for as long as the client likes.

// Helpers panic on unexpected values, like the tests that call them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{sync::Arc, time::Duration};

use bytes::Bytes;
use futures_util::{StreamExt, stream};
use mammoth_cache::{
    cache::Cache,
    key::CacheKey,
    router,
    server::{self, ServerSettings},
    store::memory::MemoryStore,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    time::timeout,
};

const KEY: &str = "0123456789abcdef0123456789abcdef";
/// Far more than the socket buffers on both ends hold.
const SIZE: usize = 64 << 20;

#[tokio::test]
async fn a_client_that_stops_reading_is_disconnected() {
    let cache = Arc::new(
        Cache::builder()
            .bounded_tier(Arc::new(MemoryStore::new(8 << 30)))
            .max_entry_size(SIZE as u64)
            .build(),
    );
    let body = stream::iter((0..SIZE / (1 << 20)).map(|_| Ok(Bytes::from(vec![7_u8; 1 << 20]))));
    cache
        .put(
            &CacheKey::gradle(KEY).unwrap(),
            Some(SIZE as u64),
            body.boxed(),
        )
        .await
        .unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let settings = ServerSettings {
        send_timeout: Duration::from_millis(300),
        ..ServerSettings::default()
    };
    let server = tokio::spawn(server::serve(
        listener,
        router(cache.clone(), None),
        cache,
        settings,
        async {
            let _ = stopped.await;
        },
    ));

    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(format!("GET /cache/{KEY} HTTP/1.1\r\nHost: test\r\n\r\n").as_bytes())
        .await
        .unwrap();
    // Read nothing for longer than the timeout.
    tokio::time::sleep(Duration::from_secs(1)).await;

    // What was already in flight arrives, then the connection ends short of the body.
    let mut received = Vec::new();
    // A reset is as good as an end here.
    let _ = timeout(Duration::from_secs(10), client.read_to_end(&mut received))
        .await
        .expect("the server kept the stalled connection open");
    assert!(
        received.len() < SIZE,
        "got all {} bytes; the connection was not cut",
        received.len()
    );

    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}
