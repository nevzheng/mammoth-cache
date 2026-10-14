//! The listener: the default `[::]` accepts IPv4 and IPv6 clients, or IPv4 alone on a host
//! without IPv6, and an IPv6 loopback address serves like any other, credentials included.

// Helpers panic on unexpected values, like the tests that call them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    net::{Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use mammoth_cache::{
    auth::{Authenticator, Scope, hash_token},
    cache::Cache,
    router, router_with_auth,
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

/// Sends `request` to `addr` and returns the status line.
async fn send(addr: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
        .await
        .expect("no response")
        .unwrap();
    let response = String::from_utf8_lossy(&response).into_owned();
    response.lines().next().unwrap_or_default().to_owned()
}

#[tokio::test]
async fn credentials_work_over_ipv6() {
    if !has_ipv6() {
        eprintln!("skipped: this host has no IPv6");
        return;
    }
    let listener = server::bind(SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))).unwrap();
    let addr = SocketAddr::from((Ipv6Addr::LOCALHOST, listener.local_addr().unwrap().port()));
    let cache = Arc::new(
        Cache::builder()
            .bounded_tier(Arc::new(MemoryStore::new(1 << 20)))
            .build(),
    );
    // Anonymous requests get nothing; `ci` may read and write.
    let auth = Authenticator::new(
        [].into_iter().collect(),
        [(
            "ci".to_owned(),
            vec![hash_token("ci-token")],
            [Scope::CacheRead, Scope::CacheWrite].into_iter().collect(),
        )],
        false,
    );
    tokio::spawn(server::serve(
        listener,
        router_with_auth(cache.clone(), None, Arc::new(auth)),
        cache,
        ServerSettings::default(),
        std::future::pending(),
    ));
    let key = "a3f5e1c09b2d4e7f8a1b6c3d5e9f0a21";
    let put = |auth: &str| {
        format!(
            "PUT /cache/{key} HTTP/1.1\r\nHost: [::1]\r\n{auth}Content-Length: 2\r\n\
             Connection: close\r\n\r\nv6"
        )
    };
    assert_eq!(send(addr, &put("")).await, "HTTP/1.1 401 Unauthorized");
    // `ci:ci-token`, base64.
    let basic = "Authorization: Basic Y2k6Y2ktdG9rZW4=\r\n";
    assert_eq!(send(addr, &put(basic)).await, "HTTP/1.1 201 Created");
    let get = format!(
        "GET /cache/{key} HTTP/1.1\r\nHost: [::1]\r\nAuthorization: Bearer ci-token\r\n\
         Connection: close\r\n\r\n"
    );
    assert_eq!(send(addr, &get).await, "HTTP/1.1 200 OK");
}
