//! The HTTP surface over a real socket: `Expect: 100-continue`, keep-alive after a drained body,
//! clients that disconnect or lie about `Content-Length`, and edge-case sizes. In-process tests
//! (`tower::ServiceExt::oneshot`) cannot see any of these, because hyper handles them.

// Helpers panic on unexpected values, like the tests that call them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{net::SocketAddr, sync::Arc, time::Duration};

use mammoth_cache::{
    auth::{Authenticator, Scope, hash_token},
    cache::Cache,
    router_with_auth,
    store::memory::MemoryStore,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

const LIMIT: u64 = 1024;
/// `ci:ci-token`, base64.
const CI: &str = "Basic Y2k6Y2ktdG9rZW4=";

/// Starts a server with a 1 KiB entry limit. Anonymous requests may read; `ci` may write.
async fn start() -> SocketAddr {
    start_with_limit(LIMIT).await
}

async fn start_with_limit(limit: u64) -> SocketAddr {
    let cache = Cache::builder()
        .bounded_tier(Arc::new(MemoryStore::new(128 << 20)))
        .max_entry_size(limit)
        .build();
    let auth = Authenticator::new(
        [Scope::CacheRead].into_iter().collect(),
        [(
            "ci".to_owned(),
            vec![hash_token("ci-token")],
            [Scope::CacheRead, Scope::CacheWrite].into_iter().collect(),
        )],
        false,
    );
    let app = router_with_auth(Arc::new(cache), None, Arc::new(auth));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// Reads one response head and returns its status code and the head.
async fn read_head(stream: &mut TcpStream) -> (u16, String) {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let n = timeout(Duration::from_secs(5), stream.read(&mut byte))
            .await
            .expect("timed out waiting for a response")
            .unwrap();
        assert_eq!(
            n,
            1,
            "connection closed mid-response: {:?}",
            String::from_utf8_lossy(&head)
        );
        head.push(byte[0]);
    }
    let head = String::from_utf8(head).unwrap();
    let status = head[9..12].parse().unwrap();
    (status, head)
}

/// Reads a whole response: status and body (`Content-Length` framed).
async fn read_response(stream: &mut TcpStream) -> (u16, Vec<u8>) {
    let (status, head) = read_head(stream).await;
    let len = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length: ")
                .map(str::to_owned)
        })
        .map_or(0, |v| v.trim().parse().unwrap());
    let mut body = vec![0_u8; len];
    stream.read_exact(&mut body).await.unwrap();
    (status, body)
}

fn put_head(key: &str, len: u64, expect: bool, auth: Option<&str>) -> String {
    let mut head = format!("PUT /cache/{key} HTTP/1.1\r\nHost: test\r\nContent-Length: {len}\r\n");
    if expect {
        head.push_str("Expect: 100-continue\r\n");
    }
    if let Some(auth) = auth {
        head.push_str("Authorization: ");
        head.push_str(auth);
        head.push_str("\r\n");
    }
    head + "\r\n"
}

async fn get(addr: SocketAddr, key: &str) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!("GET /cache/{key} HTTP/1.1\r\nHost: test\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    read_response(&mut stream).await
}

#[tokio::test]
async fn continue_is_sent_only_for_an_accepted_upload() {
    let addr = start().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(put_head("a1", 5, true, Some(CI)).as_bytes())
        .await
        .unwrap();
    let (status, _) = read_head(&mut stream).await;
    assert_eq!(status, 100);
    stream.write_all(b"hello").await.unwrap();
    assert_eq!(read_response(&mut stream).await.0, 201);
    assert_eq!(get(addr, "a1").await, (200, b"hello".to_vec()));
}

#[tokio::test]
async fn refused_uploads_are_answered_before_the_body() {
    let addr = start().await;
    let cases = [
        // Oversized Gradle entry: accepted and dropped, never sent.
        (put_head("b1", LIMIT + 1, true, Some(CI)), 200),
        // No credentials and anonymous may only read.
        (put_head("b2", 5, true, None), 401),
        // Oversized Maven object: refused.
        (
            format!(
                "PUT /maven/v1/x.jar HTTP/1.1\r\nHost: test\r\nContent-Length: {}\r\n\
                 Expect: 100-continue\r\nAuthorization: {CI}\r\n\r\n",
                LIMIT + 1
            ),
            413,
        ),
    ];
    for (head, expected) in cases {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(head.as_bytes()).await.unwrap();
        let (status, _) = read_response(&mut stream).await;
        assert_eq!(status, expected, "{head}");
    }
    assert_eq!(get(addr, "b1").await.0, 404);
}

#[tokio::test]
async fn an_oversized_body_sent_without_expect_is_drained_and_the_connection_reused() {
    let addr = start().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let body = vec![b'x'; usize::try_from(LIMIT).unwrap() * 4];
    stream
        .write_all(put_head("c1", body.len() as u64, false, Some(CI)).as_bytes())
        .await
        .unwrap();
    stream.write_all(&body).await.unwrap();
    assert_eq!(read_response(&mut stream).await.0, 200);

    // Same connection: the server read the whole body, so the next request parses.
    stream
        .write_all(put_head("c2", 2, false, Some(CI)).as_bytes())
        .await
        .unwrap();
    stream.write_all(b"ok").await.unwrap();
    assert_eq!(read_response(&mut stream).await.0, 201);
    assert_eq!(get(addr, "c1").await.0, 404);
}

/// Sends a request with a body on `stream` and returns the response status.
async fn send(stream: &mut TcpStream, head: &str, body: &[u8]) -> u16 {
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    read_response(stream).await.0
}

#[tokio::test]
async fn a_repeated_upload_is_drained_and_the_connection_reused() {
    let addr = start().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let body = vec![b'x'; 1000];
    let head = put_head("e1", body.len() as u64, false, Some(CI));
    assert_eq!(send(&mut stream, &head, &body).await, 201);
    // The key is stored, so the second upload is not needed, but its body is already sent.
    assert_eq!(send(&mut stream, &head, &body).await, 200);
    assert_eq!(
        send(&mut stream, &put_head("e2", 2, false, Some(CI)), b"ok").await,
        201
    );

    // With `Expect`, the server answers without asking for the body.
    stream
        .write_all(put_head("e1", body.len() as u64, true, Some(CI)).as_bytes())
        .await
        .unwrap();
    assert_eq!(read_response(&mut stream).await.0, 200);
}

#[tokio::test]
async fn a_refused_body_sent_without_expect_is_drained_and_the_connection_reused() {
    let addr = start().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    // Large enough that the client is still sending when the answer comes.
    let body = vec![b'x'; 1 << 20];
    let maven = |len: usize| {
        format!(
            "PUT /maven/v1/y.jar HTTP/1.1\r\nHost: test\r\nContent-Length: {len}\r\n\
             Authorization: {CI}\r\n\r\n"
        )
    };
    assert_eq!(send(&mut stream, &maven(body.len()), &body).await, 413);
    assert_eq!(send(&mut stream, &maven(2), b"ok").await, 201);
}

#[tokio::test]
async fn a_client_that_disconnects_mid_upload_stores_nothing() {
    let addr = start().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(put_head("d1", 500, false, Some(CI)).as_bytes())
        .await
        .unwrap();
    stream.write_all(&[1_u8; 100]).await.unwrap();
    drop(stream);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(get(addr, "d1").await.0, 404);
}

#[tokio::test]
async fn a_body_shorter_than_its_content_length_stores_nothing() {
    let addr = start().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(put_head("e1", 10, false, Some(CI)).as_bytes())
        .await
        .unwrap();
    stream.write_all(b"short").await.unwrap();
    stream.shutdown().await.unwrap();
    // The server sees end of input before Content-Length bytes: no response, nothing stored.
    let mut rest = Vec::new();
    let _ = timeout(Duration::from_secs(5), stream.read_to_end(&mut rest)).await;
    assert_eq!(get(addr, "e1").await.0, 404);
}

#[tokio::test]
async fn sizes_at_the_edges() {
    let addr = start().await;
    let at_limit = vec![b'y'; usize::try_from(LIMIT).unwrap()];
    for (key, body, stored) in [
        ("f0", Vec::new(), true),
        ("f1", at_limit.clone(), true),
        ("f2", [at_limit.as_slice(), b"!"].concat(), false),
    ] {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(put_head(key, body.len() as u64, false, Some(CI)).as_bytes())
            .await
            .unwrap();
        stream.write_all(&body).await.unwrap();
        let expected = if stored { 201 } else { 200 };
        assert_eq!(read_response(&mut stream).await.0, expected, "{key}");
        let (status, got) = get(addr, key).await;
        if stored {
            assert_eq!((status, got), (200, body), "{key}");
        } else {
            assert_eq!(status, 404, "{key}");
        }
    }
}

#[tokio::test]
async fn a_slow_answer_to_expect_continue_still_reads_the_body() {
    let addr = start_with_limit(4 << 20).await;
    // Large enough that hyper does not read it on its own.
    let size = 1 << 20;
    let half = vec![b'1'; size / 2];
    // The first upload of a key holds it for 3.5 s, sending half its body now, half later.
    let mut first = TcpStream::connect(addr).await.unwrap();
    first
        .write_all(put_head("c7", size as u64, false, Some(CI)).as_bytes())
        .await
        .unwrap();
    first.write_all(&half).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // A second upload of the key asks first, but like Apache HttpClient sends the body anyway
    // after 3 s without `100 Continue`. The server is still waiting on the first upload then.
    let mut second = TcpStream::connect(addr).await.unwrap();
    second
        .write_all(put_head("c7", size as u64, true, Some(CI)).as_bytes())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (reader, mut writer) = second.into_split();
    // In the background: the server reads it only once it has decided.
    let sent =
        tokio::spawn(async move { writer.write_all(&vec![b'2'; size]).await.map(|()| writer) });
    tokio::time::sleep(Duration::from_millis(500)).await;
    first.write_all(&half).await.unwrap();
    assert_eq!(read_response(&mut first).await.0, 201);

    // The second store finds the entry. Its body was read, so the connection carries on.
    let writer = sent
        .await
        .unwrap()
        .expect("the server stopped reading the body");
    let mut second = reader.reunite(writer).unwrap();
    let (mut status, _) = read_head(&mut second).await;
    if status == 100 {
        status = read_response(&mut second).await.0;
    }
    assert_eq!(status, 200);
    assert_eq!(
        send(&mut second, &put_head("c8", 2, false, Some(CI)), b"ok").await,
        201
    );
}
