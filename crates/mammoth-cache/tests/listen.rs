//! The listener: the default `[::]` accepts IPv4 and IPv6 clients, or IPv4 alone on a host
//! without IPv6, and an IPv6 loopback address serves like any other.

// Helpers panic on unexpected values, like the tests that call them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    net::{Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use mammoth_cache::{
    cache::Cache,
    router,
    server::{self, ServerSettings},
    store::memory::MemoryStore,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

fn serve(listener: TcpListener) {
    let cache = Arc::new(
        Cache::builder()
            .bounded_tier(Arc::new(MemoryStore::new(1 << 20)))
            .build(),
    );
    tokio::spawn(server::serve(
        listener,
        router(cache.clone(), None),
        cache,
        ServerSettings::default(),
        std::future::pending(),
    ));
}

/// Sends `GET /ping` to `addr` and returns the status line.
async fn ping(addr: SocketAddr) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET /ping HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
        .await
        .expect("no response")
        .unwrap();
    let response = String::from_utf8_lossy(&response).into_owned();
    response.lines().next().unwrap_or_default().to_owned()
}

/// Whether this host can use IPv6 at all; CI runners can, some containers cannot.
fn has_ipv6() -> bool {
    std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).is_ok()
}

#[tokio::test]
async fn the_default_listen_address_serves_ipv4_and_ipv6_clients() {
    let listener = server::bind(SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))).unwrap();
    let local = listener.local_addr().unwrap();
    serve(listener);
    assert_eq!(
        ping(SocketAddr::from(([127, 0, 0, 1], local.port()))).await,
        "HTTP/1.1 200 OK"
    );
    if has_ipv6() {
        assert!(local.is_ipv6(), "fell back to {local} on a host with IPv6");
        assert_eq!(
            ping(SocketAddr::from((Ipv6Addr::LOCALHOST, local.port()))).await,
            "HTTP/1.1 200 OK"
        );
    } else {
        assert!(local.is_ipv4(), "{local}");
    }
}

#[tokio::test]
async fn an_ipv6_loopback_address_serves() {
    if !has_ipv6() {
        eprintln!("skipped: this host has no IPv6");
        return;
    }
    let listener = server::bind("[::1]:0".parse().unwrap()).unwrap();
    let local = listener.local_addr().unwrap();
    serve(listener);
    assert_eq!(ping(local).await, "HTTP/1.1 200 OK");
}
