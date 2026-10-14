//! The access log (design doc 0018): one line on `mammoth::access` per HTTP request and gRPC call,
//! with what it did to the cache, who sent it and from where.

// Helpers panic on unexpected values, like the tests that call them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use http_body_util::BodyExt;
use mammoth_cache::{
    access,
    auth::{Authenticator, Scope, Scopes, hash_token, new_token},
    cache::Cache,
    config::Protocols,
    logging::JsonFormat,
    reapi::{
        self,
        proto::build::bazel::remote::execution::v2::{
            Digest, FindMissingBlobsRequest, GetActionResultRequest,
            action_cache_client::ActionCacheClient,
            content_addressable_storage_client::ContentAddressableStorageClient,
        },
    },
    router_with,
    server::{self, ServerSettings},
    store::memory::MemoryStore,
};
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tonic::{Code, Request as GrpcRequest, metadata::MetadataValue, transport::Endpoint};
use tower::ServiceExt;
use tracing::{Level, subscriber::DefaultGuard};

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Captured {
    /// Captures this thread's log lines as JSON while the guard lives.
    fn install() -> (Self, DefaultGuard) {
        let captured = Self::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(Level::INFO)
            .event_format(JsonFormat)
            .with_writer(move || writer.clone())
            .finish();
        (captured, tracing::subscriber::set_default(subscriber))
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    /// The access lines so far.
    fn access(&self) -> Vec<Value> {
        self.text()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|line| line["target"] == access::TARGET)
            .collect()
    }

    /// Waits for the `n`th access line, written when its response ends.
    async fn nth(&self, n: usize) -> Value {
        for _ in 0..500 {
            if let Some(line) = self.access().get(n) {
                return line.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no access line {n}: {}", self.text());
    }
}

const KEY: &str = "4f1c9a0e6b2d7c3a";

fn auth(token: &str) -> Authenticator {
    Authenticator::new(
        Scopes::default(),
        [(
            "ci".to_owned(),
            vec![hash_token(token)],
            [Scope::CacheRead, Scope::CacheWrite]
                .into_iter()
                .collect::<Scopes>(),
        )],
        true,
    )
}

fn cache() -> Arc<Cache> {
    Arc::new(
        Cache::builder()
            .bounded_tier(Arc::new(MemoryStore::new(1 << 20)))
            .build(),
    )
}

fn app(token: &str) -> Router {
    router_with(cache(), None, Arc::new(auth(token)), Protocols::default())
}

async fn send(app: &Router, method: Method, path: &str, credentials: &str, body: &'static str) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, credentials)
        .header(header::USER_AGENT, "Gradle/8.10 (Linux;6.1;amd64)")
        .body(Body::from(body))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let _ = response.into_body().collect().await;
}

#[tokio::test]
async fn http_lines_say_what_each_request_did() {
    let (log, _guard) = Captured::install();
    let token = new_token().unwrap();
    let app = app(&token);
    let bearer = format!("Bearer {token}");
    let path = format!("/cache/{KEY}");

    send(&app, Method::GET, &path, &bearer, "").await;
    send(&app, Method::PUT, &path, &bearer, "entry").await;
    send(&app, Method::PUT, &path, &bearer, "entry").await;
    send(&app, Method::GET, &path, &bearer, "").await;
    send(&app, Method::HEAD, &path, &bearer, "").await;
    send(&app, Method::GET, &path, "Bearer wrong", "").await;
    send(&app, Method::GET, "/ping", "", "").await;
    send(&app, Method::GET, "/metrics", "", "").await;

    let lines = log.access();
    assert_eq!(lines.len(), 6, "probes are not logged: {lines:#?}");
    let expected = [
        ("GET", 404, Some("miss"), "ci"),
        ("PUT", 201, Some("stored"), "ci"),
        ("PUT", 200, Some("exists"), "ci"),
        ("GET", 200, Some("hit"), "ci"),
        ("HEAD", 200, Some("hit"), "ci"),
        ("GET", 401, Some("refused"), "anonymous"),
    ];
    for (line, (method, status, result, user)) in lines.iter().zip(expected) {
        assert_eq!(line["severity"], "INFO");
        assert_eq!(line["message"], "request");
        assert_eq!(line["http.request.method"], method, "{line}");
        assert_eq!(line["http.response.status_code"], status, "{line}");
        assert_eq!(line["mammoth.cache.result"].as_str(), result, "{line}");
        assert_eq!(line["user.name"], user, "{line}");
        assert_eq!(line["url.path"], path);
        assert_eq!(line["http.route"], "/cache/{key}");
        assert_eq!(line["mammoth.protocol"], "gradle");
        assert_eq!(line["user_agent.original"], "Gradle/8.10 (Linux;6.1;amd64)");
        assert!(line["mammoth.duration_ms"].as_f64().unwrap() >= 0.0);
        assert!(line.get("mammoth.aborted").is_none(), "{line}");
        // No connection, so no address.
        assert!(line.get("client.address").is_none(), "{line}");
    }
    assert_eq!(lines[1]["http.request.body.size"], 5);
    assert_eq!(lines[1]["mammoth.cache.key"], format!("gradle/{KEY}"));
    assert_eq!(lines[3]["http.response.body.size"], 5);
    assert_eq!(lines[3]["mammoth.cache.tier"], "memory");
    // Refused before the handler: no key.
    assert!(lines[5].get("mammoth.cache.key").is_none());
    assert!(!log.text().contains(&token));
}

#[tokio::test]
async fn a_response_the_client_drops_is_aborted() {
    let (log, _guard) = Captured::install();
    let token = new_token().unwrap();
    let app = app(&token);
    let bearer = format!("Bearer {token}");
    let path = format!("/cache/{KEY}");
    send(&app, Method::PUT, &path, &bearer, "entry").await;

    let request = Request::builder()
        .uri(&path)
        .header(header::AUTHORIZATION, &bearer)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(log.access().len(), 1, "written when the response ends");
    drop(response);

    let line = log.nth(1).await;
    assert_eq!(line["mammoth.aborted"], true);
    assert_eq!(line["http.response.body.size"], 0);
    assert_eq!(line["mammoth.cache.result"], "hit");
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

async fn raw_get(addr: SocketAddr, forwarded_for: &str) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "GET /cache/{KEY} HTTP/1.1\r\nHost: cache\r\nX-Forwarded-For: {forwarded_for}\r\n\
         Connection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    assert!(response.starts_with(b"HTTP/1.1 404"));
}

#[tokio::test]
async fn the_client_is_found_behind_trusted_proxies() {
    // The process's only configuration: tests in this file trust loopback as a proxy.
    access::configure(true, vec!["127.0.0.0/8".parse().unwrap()]);
    let (log, _guard) = Captured::install();
    let cache = cache();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = mammoth_cache::router(cache.clone(), None);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(server::serve(listener, app, cache, settings(), async {
        let _ = stopped.await;
    }));

    // A client can put anything on the left; the right-most untrusted entry is the client.
    raw_get(addr, "6.6.6.6, 203.0.113.9, 127.0.0.5").await;
    let line = log.nth(0).await;
    assert_eq!(line["client.address"], "203.0.113.9", "{line}");
    assert_eq!(line["network.peer.address"], "127.0.0.1");
    assert_eq!(line["mammoth.cache.result"], "miss");

    raw_get(addr, "127.0.0.9").await;
    let line = log.nth(1).await;
    assert_eq!(line["client.address"], "127.0.0.9", "{line}");

    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}

fn digest(byte: u8) -> Digest {
    Digest {
        hash: format!("{byte:02x}").repeat(32),
        size_bytes: 3,
    }
}

/// A gRPC request carrying `token`.
fn with<T>(token: &str, message: T) -> GrpcRequest<T> {
    let mut request = GrpcRequest::new(message);
    let value = MetadataValue::try_from(format!("Bearer {token}")).unwrap();
    request.metadata_mut().insert("authorization", value);
    request
}

#[tokio::test]
async fn grpc_lines_say_what_each_call_did() {
    let (log, _guard) = Captured::install();
    let token = new_token().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let routes = reapi::routes(cache(), Arc::new(auth(&token)), settings());
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(reapi::serve(listener, None, routes, settings(), async {
        let _ = stopped.await;
    }));
    let channel = Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();

    let mut cas = ContentAddressableStorageClient::new(channel.clone());
    let find = FindMissingBlobsRequest {
        blob_digests: vec![digest(1), digest(2), digest(3)],
        ..Default::default()
    };
    cas.find_missing_blobs(with(&token, find)).await.unwrap();
    let line = log.nth(0).await;
    assert_eq!(line["message"], "call");
    assert_eq!(line["rpc.system"], "grpc");
    assert_eq!(
        line["rpc.service"],
        "build.bazel.remote.execution.v2.ContentAddressableStorage"
    );
    assert_eq!(line["rpc.method"], "FindMissingBlobs");
    assert_eq!(line["rpc.grpc.status_code"], 0);
    assert_eq!(line["mammoth.digests"], 3);
    assert_eq!(line["mammoth.protocol"], "reapi");
    assert_eq!(line["user.name"], "ci");
    assert_eq!(line["client.address"], "127.0.0.1");
    assert!(line.get("mammoth.cache.key").is_none(), "{line}");

    let mut ac = ActionCacheClient::new(channel);
    let get = GetActionResultRequest {
        action_digest: Some(digest(4)),
        ..Default::default()
    };
    let status = ac
        .get_action_result(with(&token, get.clone()))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::NotFound);
    let line = log.nth(1).await;
    assert_eq!(line["rpc.method"], "GetActionResult");
    assert_eq!(line["rpc.grpc.status_code"], 5);
    assert_eq!(line["mammoth.cache.result"], "miss");
    assert_eq!(
        line["mammoth.cache.key"],
        format!("bazel-ac/{}", "04".repeat(32))
    );

    let wrong = new_token().unwrap();
    let status = ac.get_action_result(with(&wrong, get)).await.unwrap_err();
    assert_eq!(status.code(), Code::Unauthenticated);
    let line = log.nth(2).await;
    assert_eq!(line["rpc.grpc.status_code"], 16);
    assert_eq!(line["mammoth.cache.result"], "refused");
    assert_eq!(line["user.name"], "anonymous");

    let text = log.text();
    assert!(!text.contains(&token) && !text.contains(&wrong));
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}
