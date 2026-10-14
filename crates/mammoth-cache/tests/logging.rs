//! The log output (design doc 0018): every line is one JSON object in the documented shape, and
//! no line ever carries a token, whichever protocol or credentials format the client used.

// Helpers panic on unexpected values, like the tests that call them.
#![allow(clippy::unwrap_used)]

use std::{
    io,
    sync::{Arc, Mutex},
};

use axum::{
    Router,
    body::Body,
    http::{Method, Request, header},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use http_body_util::BodyExt;
use mammoth_cache::{
    auth::{Authenticator, Scope, Scopes, hash_token, new_token},
    cache::Cache,
    config::Protocols,
    logging::JsonFormat,
    router_with,
    store::memory::MemoryStore,
};
use serde_json::Value;
use tower::ServiceExt;
use tracing::Level;

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
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

/// A node with one user, `ci`, who holds `token`, serving every HTTP protocol.
fn app(token: &str) -> Router {
    let cache = Cache::builder()
        .bounded_tier(Arc::new(MemoryStore::new(1 << 20)))
        .build();
    let auth = Authenticator::new(
        Scopes::default(),
        [(
            "ci".to_owned(),
            vec![hash_token(token)],
            [Scope::CacheRead, Scope::CacheWrite, Scope::AdminRead]
                .into_iter()
                .collect::<Scopes>(),
        )],
        true,
    );
    let protocols = Protocols {
        bazel: true,
        ..Protocols::default()
    };
    router_with(Arc::new(cache), None, Arc::new(auth), protocols)
}

const DIGEST: &str = "2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae";

/// Every route with every credentials format, the right token and wrong ones.
async fn traffic(app: &Router, token: &str, other: &str) {
    let paths = [
        "/cache/a3f5e1c09b2d4e7f".to_owned(),
        "/maven/v1/com/acme/app/1.0/build.zip".to_owned(),
        format!("/bazel/cas/{DIGEST}"),
        format!("/bazel/ac/{DIGEST}"),
        "/v1/serverStatus".to_owned(),
        "/metrics".to_owned(),
        "/no/such/route".to_owned(),
    ];
    let credentials = [
        format!("Bearer {token}"),
        format!("Basic {}", STANDARD.encode(format!("ci:{token}"))),
        format!("Basic {}", STANDARD.encode(format!(":{token}"))),
        format!("Basic {}", STANDARD.encode(format!("{token}:{other}"))),
        format!("Bearer {other}"),
        format!("Basic {}", STANDARD.encode(format!("ci:{other}"))),
        format!("Token {token}"),
    ];
    for path in &paths {
        for method in [Method::GET, Method::HEAD, Method::PUT] {
            for credentials in &credentials {
                let request = Request::builder()
                    .method(method.clone())
                    .uri(path)
                    .header(header::AUTHORIZATION, credentials)
                    .header(header::CONTENT_LENGTH, 5)
                    .body(if method == Method::PUT {
                        Body::from("entry")
                    } else {
                        Body::empty()
                    })
                    .unwrap();
                let response = app.clone().oneshot(request).await.unwrap();
                let _ = response.into_body().collect().await;
            }
        }
    }
}

#[tokio::test]
async fn no_line_carries_a_token() {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(Level::TRACE)
        .event_format(JsonFormat)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let mut tokens = Vec::new();
    for _ in 0..8 {
        let token = new_token().unwrap();
        let other = new_token().unwrap();
        traffic(&app(&token), &token, &other).await;
        tokens.extend([token, other]);
    }
    tracing::info!("end of traffic");

    let text = captured.text();
    assert!(text.contains("end of traffic"), "nothing was captured");
    for token in &tokens {
        assert!(!text.contains(token.as_str()), "a token reached the log");
        let half = &token[..token.len() / 2];
        assert!(!text.contains(half), "part of a token reached the log");
    }
    for line in text.lines() {
        let line: Value = serde_json::from_str(line).unwrap();
        for key in ["timestamp", "severity", "target", "message"] {
            assert!(line.get(key).is_some(), "{key} missing from {line}");
        }
    }
}
