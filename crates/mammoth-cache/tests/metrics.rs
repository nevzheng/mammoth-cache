//! `/metrics` with a Prometheus recorder installed, checked against the catalog in design doc
//! 0004. Its own test binary, because the recorder is process-wide.
//!
//! The [`CATALOG`] table below is the test's copy of 0004: every metric, its type, its label keys,
//! the allowed values of each bounded label, and whether it is live on `main`. An instrumentation
//! pull request flips rows to live; a metric the catalog does not list fails the test. Design doc
//! 0015 explains the plan.

// Helpers panic on unexpected values, like the tests that call them.
#![allow(clippy::unwrap_used)]

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
    response::Response,
};
use http_body_util::BodyExt;
use mammoth_cache::{
    auth::{Authenticator, Scope, Scopes, hash_token},
    cache::{BucketWrite, Cache, OversizePolicy},
    router_with_auth,
    store::{
        bucket::{BucketConfig, BucketStore},
        disk::{DiskConfig, DiskStore},
        memory::{EvictionPolicy, MemoryConfig, MemoryStore},
    },
};
use metrics_exporter_prometheus::PrometheusHandle;
use serde_json::Value;
use tower::ServiceExt;

/// Serialises the tests that read counters: the recorder is shared by every test in this binary,
/// so deltas taken while another workload runs would be off.
async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

/// The node's Prometheus recorder, installed once for the whole test binary.
fn recorder() -> PrometheusHandle {
    mammoth_cache::telemetry::test_recorder()
}

// ---------------------------------------------------------------------------------------------
// The catalog (design doc 0004)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Counter,
    Gauge,
    Histogram,
}

impl Kind {
    fn prometheus_type(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
        }
    }
}

/// A label key and, for bounded labels, every value it may take. `None` means the values are
/// normalised rather than enumerated (`route`, `code`, `gradle_version`, `version`, `revision`).
struct Label(&'static str, Option<&'static [&'static str]>);

struct Metric {
    name: &'static str,
    kind: Kind,
    labels: &'static [Label],
    /// Whether `main` records it today. A live metric must appear after the workload below.
    live: bool,
}

const TIERS: &[&str] = &["memory", "disk", "bucket"];
const CLIENTS: &[&str] = &["gradle", "maven", "other"];
const METHODS: &[&str] = &[
    "GET", "HEAD", "PUT", "POST", "DELETE", "OPTIONS", "MKCOL", "other",
];

const CATALOG: &[Metric] = &[
    // Cache outcomes
    Metric {
        name: "mammoth_cache_lookups_total",
        kind: Kind::Counter,
        labels: &[
            Label("result", Some(&["hit", "miss"])),
            // Empty on a miss.
            Label("tier", Some(&["memory", "disk", "bucket", ""])),
        ],
        live: true,
    },
    Metric {
        name: "mammoth_cache_stores_total",
        kind: Kind::Counter,
        labels: &[Label(
            "result",
            Some(&["stored", "exists", "too_large", "client_error", "error"]),
        )],
        live: true,
    },
    Metric {
        name: "mammoth_cache_entry_size_bytes",
        kind: Kind::Histogram,
        labels: &[],
        live: true,
    },
    Metric {
        name: "mammoth_cache_integrity_errors_total",
        kind: Kind::Counter,
        labels: &[Label("tier", Some(TIERS))],
        // Recorded by the disk tier, but only a corrupted file produces one.
        live: false,
    },
    // Per tier
    Metric {
        name: "mammoth_tier_reads_total",
        kind: Kind::Counter,
        labels: &[
            Label("tier", Some(TIERS)),
            Label("result", Some(&["hit", "miss", "error"])),
        ],
        live: true,
    },
    Metric {
        name: "mammoth_tier_read_duration_seconds",
        kind: Kind::Histogram,
        labels: &[Label("tier", Some(TIERS))],
        live: true,
    },
    Metric {
        name: "mammoth_tier_writes_total",
        kind: Kind::Counter,
        labels: &[
            Label("tier", Some(TIERS)),
            Label("result", Some(&["ok", "error"])),
        ],
        live: true,
    },
    Metric {
        name: "mammoth_tier_write_duration_seconds",
        kind: Kind::Histogram,
        labels: &[Label("tier", Some(TIERS))],
        live: true,
    },
    Metric {
        name: "mammoth_tier_read_bytes_total",
        kind: Kind::Counter,
        labels: &[Label("tier", Some(TIERS))],
        live: true,
    },
    Metric {
        name: "mammoth_tier_written_bytes_total",
        kind: Kind::Counter,
        labels: &[Label("tier", Some(TIERS))],
        live: true,
    },
    Metric {
        name: "mammoth_tier_promotions_total",
        kind: Kind::Counter,
        labels: &[Label("from", Some(TIERS)), Label("to", Some(TIERS))],
        live: true,
    },
    Metric {
        name: "mammoth_tier_evictions_total",
        kind: Kind::Counter,
        labels: &[
            Label("tier", Some(TIERS)),
            Label("reason", Some(&["size", "age"])),
        ],
        live: true,
    },
    Metric {
        name: "mammoth_bucket_upload_queue_entries",
        kind: Kind::Gauge,
        labels: &[],
        live: true,
    },
    Metric {
        name: "mammoth_bucket_uploads_total",
        kind: Kind::Counter,
        labels: &[Label("result", Some(&["ok", "skipped", "error"]))],
        live: true,
    },
    Metric {
        name: "mammoth_bucket_uploads_dropped_total",
        kind: Kind::Counter,
        labels: &[Label(
            "reason",
            Some(&["queue-full", "shutdown", "bucket-down"]),
        )],
        // Only a full upload queue, a drain or a bucket outage produces one.
        live: false,
    },
    Metric {
        name: "mammoth_shutting_down",
        kind: Kind::Gauge,
        labels: &[],
        // 0 from the start, 1 while the node drains.
        live: true,
    },
    Metric {
        name: "mammoth_tier_up",
        kind: Kind::Gauge,
        labels: &[Label("tier", Some(TIERS))],
        live: true,
    },
    // Current population
    Metric {
        name: "mammoth_tier_entries",
        kind: Kind::Gauge,
        labels: &[Label("tier", Some(&["memory", "disk"]))],
        live: true,
    },
    Metric {
        name: "mammoth_tier_size_bytes",
        kind: Kind::Gauge,
        labels: &[Label("tier", Some(&["memory", "disk"]))],
        live: true,
    },
    Metric {
        name: "mammoth_tier_capacity_bytes",
        kind: Kind::Gauge,
        labels: &[Label("tier", Some(&["memory", "disk"]))],
        live: true,
    },
    Metric {
        name: "mammoth_tier_oldest_entry_age_seconds",
        kind: Kind::Gauge,
        labels: &[Label("tier", Some(&["memory", "disk"]))],
        live: false,
    },
    Metric {
        name: "mammoth_disk_free_bytes",
        kind: Kind::Gauge,
        labels: &[],
        // Set by the probe `DiskStore::open` runs.
        live: true,
    },
    // TLS (design doc 0014), only with `[tls]` set.
    Metric {
        name: "mammoth_tls_certificate_expiry_seconds",
        kind: Kind::Gauge,
        labels: &[],
        live: false,
    },
    Metric {
        name: "mammoth_tls_reloads_total",
        kind: Kind::Counter,
        labels: &[Label("result", Some(&["ok", "error"]))],
        live: false,
    },
    Metric {
        name: "mammoth_tls_handshake_failures_total",
        kind: Kind::Counter,
        labels: &[Label("reason", Some(&["error", "timeout"]))],
        live: false,
    },
    // Serving
    Metric {
        name: "mammoth_http_requests_total",
        kind: Kind::Counter,
        labels: &[
            Label("route", None),
            Label("method", Some(METHODS)),
            Label("code", None),
            Label("client", Some(CLIENTS)),
        ],
        live: true,
    },
    Metric {
        name: "mammoth_gradle_requests_total",
        kind: Kind::Counter,
        labels: &[Label("gradle_version", None)],
        live: true,
    },
    Metric {
        name: "mammoth_grpc_requests_total",
        kind: Kind::Counter,
        labels: &[Label("method", None), Label("code", None)],
        live: true,
    },
    Metric {
        name: "mammoth_grpc_request_duration_seconds",
        kind: Kind::Histogram,
        labels: &[Label("method", None)],
        live: true,
    },
    Metric {
        name: "mammoth_grpc_requests_in_flight",
        kind: Kind::Gauge,
        labels: &[Label("method", None)],
        live: true,
    },
    Metric {
        name: "mammoth_http_request_duration_seconds",
        kind: Kind::Histogram,
        labels: &[Label("route", None), Label("method", Some(METHODS))],
        live: true,
    },
    Metric {
        name: "mammoth_http_requests_in_flight",
        kind: Kind::Gauge,
        labels: &[Label("route", None)],
        live: true,
    },
    Metric {
        name: "mammoth_auth_failures_total",
        kind: Kind::Counter,
        labels: &[Label(
            "reason",
            Some(&[
                "CREDENTIALS_MISSING",
                "CREDENTIALS_INVALID",
                "SCOPE_MISSING",
            ]),
        )],
        live: true,
    },
    Metric {
        name: "mammoth_log_lines_dropped_total",
        kind: Kind::Counter,
        labels: &[],
        live: true,
    },
    Metric {
        name: "mammoth_build_info",
        kind: Kind::Gauge,
        labels: &[Label("version", None), Label("revision", None)],
        live: true,
    },
];

fn catalog(name: &str) -> Option<&'static Metric> {
    CATALOG.iter().find(|m| m.name == name)
}

// ---------------------------------------------------------------------------------------------
// A small parser for the Prometheus text exposition
// ---------------------------------------------------------------------------------------------

type Labels = BTreeMap<String, String>;

#[derive(Debug, Default)]
struct Scrape {
    /// `# TYPE` declarations, by family name.
    types: BTreeMap<String, String>,
    /// Every sample: name, labels, value.
    samples: Vec<(String, Labels, f64)>,
}

impl Scrape {
    fn parse(text: &str) -> Self {
        let mut scrape = Self::default();
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if let Some(rest) = line.strip_prefix("# TYPE ") {
                let (name, kind) = rest.split_once(' ').unwrap();
                scrape.types.insert(name.to_owned(), kind.trim().to_owned());
            } else if !line.starts_with('#') {
                scrape.samples.push(parse_sample(line));
            }
        }
        scrape
    }

    /// The family a sample belongs to: histogram samples drop `_bucket`, `_sum` and `_count`.
    fn family(&self, sample: &str) -> String {
        for suffix in ["_bucket", "_sum", "_count"] {
            if let Some(base) = sample.strip_suffix(suffix)
                && self.types.get(base).map(String::as_str) == Some("histogram")
            {
                return base.to_owned();
            }
        }
        sample.to_owned()
    }

    fn samples_of<'a>(
        &'a self,
        family: &'a str,
    ) -> impl Iterator<Item = &'a (String, Labels, f64)> {
        self.samples
            .iter()
            .filter(move |(n, _, _)| self.family(n) == family)
    }

    /// [`Self::value`] as a whole number, for counters and entry gauges.
    #[allow(clippy::cast_possible_truncation)]
    fn count(&self, name: &str, want: &[(&str, &str)]) -> i64 {
        self.value(name, want).round() as i64
    }

    /// Sum of the samples of `name` whose labels include `want`.
    fn value(&self, name: &str, want: &[(&str, &str)]) -> f64 {
        self.samples
            .iter()
            .filter(|(n, labels, _)| {
                n == name
                    && want
                        .iter()
                        .all(|(k, v)| labels.get(*k).map(String::as_str) == Some(*v))
            })
            .map(|(_, _, v)| v)
            .sum()
    }
}

fn parse_sample(line: &str) -> (String, Labels, f64) {
    let (head, value) = line.rsplit_once(' ').unwrap();
    let value: f64 = value
        .parse()
        .unwrap_or_else(|_| panic!("bad value in {line:?}"));
    let Some((name, rest)) = head.split_once('{') else {
        return (head.to_owned(), Labels::new(), value);
    };
    let body = rest
        .strip_suffix('}')
        .unwrap_or_else(|| panic!("unterminated labels in {line:?}"));
    let mut labels = Labels::new();
    let mut chars = body.chars().peekable();
    while chars.peek().is_some() {
        let key: String = chars.by_ref().take_while(|c| *c != '=').collect();
        assert_eq!(
            chars.next(),
            Some('"'),
            "label value not quoted in {line:?}"
        );
        let mut val = String::new();
        loop {
            match chars.next().unwrap() {
                '\\' => val.push(chars.next().unwrap()),
                '"' => break,
                c => val.push(c),
            }
        }
        labels.insert(key.trim().to_owned(), val);
        if chars.peek() == Some(&',') {
            chars.next();
        }
    }
    (name.to_owned(), labels, value)
}

// ---------------------------------------------------------------------------------------------
// The workload
// ---------------------------------------------------------------------------------------------

const GRADLE_AGENT: &str = "Gradle/8.10 (Linux;6.1;amd64) (Eclipse Adoptium;17.0.9;17.0.9+9)";
const MAVEN_AGENT: &str = "Apache-Maven/3.9.9 (Java 21.0.4; Linux 6.8.0)";
/// Three [`ENTRY`]-sized entries do not fit, so storing them evicts one.
const MEMORY_CAPACITY: u64 = 8192;
const ENTRY: usize = 3000;
const MAX_ENTRY: u64 = 16 * 1024;
const UPLOAD_QUEUE: usize = 8;
const TOKEN: &str = "Bearer ci-token";

/// All three tiers, with a memory tier so small that a second entry evicts the first.
async fn three_tier_app(dir: &std::path::Path, auth: Authenticator) -> Router {
    // Before the stores: `DiskStore::open` sets `mammoth_disk_free_bytes` from its first probe,
    // and a gauge set before the recorder exists goes to the no-op recorder and is lost.
    let handle = recorder();
    let disk = DiskStore::open(DiskConfig {
        path: dir.to_path_buf(),
        capacity_bytes: 1 << 20,
        max_entry_age: None,
        free_space_buffer: 0,
    })
    .await
    .unwrap();
    let bucket = BucketStore::open(&BucketConfig::new("memory:///")).unwrap();
    let memory: MemoryStore = MemoryStore::with_config(MemoryConfig {
        capacity_bytes: MEMORY_CAPACITY,
        max_entry_bytes: MAX_ENTRY,
        eviction: EvictionPolicy::default(),
    })
    .unwrap();
    let cache = Cache::builder()
        .bounded_tier(Arc::new(memory))
        .bounded_tier(Arc::new(disk))
        .bucket_tier(
            Arc::new(bucket),
            BucketWrite::Async {
                queue: UPLOAD_QUEUE,
            },
        )
        .max_entry_size(MAX_ENTRY)
        .oversize(OversizePolicy::Drop)
        .build();
    let (cache, auth) = (Arc::new(cache), Arc::new(auth));
    let settings = mammoth_cache::server::ServerSettings::default();
    let grpc =
        mammoth_cache::reapi::routes(cache.clone(), auth.clone(), settings).into_axum_router();
    let http = router_with_auth(cache, Some(handle), auth);
    // One app for both ports, as the tests send requests rather than open connections: gRPC
    // calls by their content type, everything else to the HTTP routes.
    Router::new().fallback_service(tower::service_fn(move |request: Request<Body>| {
        let grpc_call = request
            .headers()
            .get(header::CONTENT_TYPE)
            .is_some_and(|t| t.as_bytes().starts_with(b"application/grpc"));
        let app = if grpc_call {
            grpc.clone()
        } else {
            http.clone()
        };
        app.oneshot(request)
    }))
}

/// A gRPC call of `method` with an empty message, and its trailers' `grpc-status`.
async fn grpc_call(app: &Router, method: &str, authorization: Option<&str>) -> String {
    let mut request = Request::post(method)
        .header(header::CONTENT_TYPE, "application/grpc")
        .header("te", "trailers");
    if let Some(value) = authorization {
        request = request.header(header::AUTHORIZATION, value);
    }
    // One uncompressed, empty message.
    let response = send(app, request.body(Body::from(vec![0_u8; 5])).unwrap()).await;
    if let Some(status) = response.headers().get("grpc-status") {
        let status = status.to_str().unwrap().to_owned();
        drop(response.into_body().collect().await.unwrap());
        return status;
    }
    let collected = response.into_body().collect().await.unwrap();
    collected.trailers().unwrap()["grpc-status"]
        .to_str()
        .unwrap()
        .to_owned()
}

const FIND_MISSING: &str =
    "/build.bazel.remote.execution.v2.ContentAddressableStorage/FindMissingBlobs";

fn authenticator() -> Authenticator {
    Authenticator::new(
        [Scope::CacheRead, Scope::CacheWrite]
            .into_iter()
            .collect::<Scopes>(),
        [(
            "ci".to_owned(),
            vec![hash_token("ci-token")],
            [Scope::CacheRead, Scope::CacheWrite, Scope::AdminRead]
                .into_iter()
                .collect::<Scopes>(),
        )],
        false,
    )
}

async fn send(app: &Router, request: Request<Body>) -> Response {
    app.clone().oneshot(request).await.unwrap()
}

async fn bytes(response: Response) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

async fn text(response: Response) -> String {
    String::from_utf8(bytes(response).await).unwrap()
}

fn gradle_get(key: &str) -> Request<Body> {
    Request::get(format!("/cache/{key}"))
        .header(header::USER_AGENT, GRADLE_AGENT)
        .header("x-gradle-version", "8.10.2")
        .body(Body::empty())
        .unwrap()
}

fn gradle_put(key: &str, size: usize) -> Request<Body> {
    Request::put(format!("/cache/{key}"))
        .header(header::USER_AGENT, GRADLE_AGENT)
        .header("x-gradle-version", "8.10.2")
        .header(
            header::CONTENT_TYPE,
            "application/vnd.gradle.build-cache-artifact.v2",
        )
        .header(header::CONTENT_LENGTH, size)
        .body(Body::from(vec![0xAB; size]))
        .unwrap()
}

async fn scrape(app: &Router) -> Scrape {
    let response = send(app, Request::get("/metrics").body(Body::empty()).unwrap()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/plain; version=0.0.4"
    );
    Scrape::parse(&text(response).await)
}

/// Exercises every path that records a metric: stores, a hit from memory and from disk, a miss,
/// an eviction and promotion, uploads to the bucket, an oversized drop, a rejected credential, and
/// Gradle, Maven and other clients.
/// `before` is a scrape taken before the workload, so the wait for background work compares deltas.
async fn workload(app: &Router, before: &Scrape) {
    const K1: &str = "a3f5e1c09b2d4e7f8a1b6c3d5e9f0a21";
    const EVICTED: [&str; 3] = [
        "a3f5e1c09b2d4e7f8a1b6c3d5e9f0a22",
        "a3f5e1c09b2d4e7f8a1b6c3d5e9f0a23",
        "a3f5e1c09b2d4e7f8a1b6c3d5e9f0a24",
    ];
    const MISSING: &str = "a3f5e1c09b2d4e7f8a1b6c3d5e9f0a99";

    // Store, then a memory hit.
    assert_eq!(
        send(app, gradle_put(K1, 64)).await.status(),
        StatusCode::CREATED
    );
    assert_eq!(send(app, gradle_get(K1)).await.status(), StatusCode::OK);
    // A miss.
    assert_eq!(
        send(app, gradle_get(MISSING)).await.status(),
        StatusCode::NOT_FOUND
    );
    // More than the memory tier holds: one is evicted, so reading all three back includes a disk
    // hit that promotes.
    for key in EVICTED {
        assert_eq!(
            send(app, gradle_put(key, ENTRY)).await.status(),
            StatusCode::CREATED
        );
    }
    for key in EVICTED {
        let response = send(app, gradle_get(key)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(bytes(response).await.len(), ENTRY);
    }
    // An oversized entry, accepted and dropped.
    let size = usize::try_from(MAX_ENTRY).unwrap() + 1;
    assert_eq!(
        send(app, gradle_put(K1, size)).await.status(),
        StatusCode::OK
    );
    // A rejected credential.
    let request = Request::get(format!("/cache/{K1}"))
        .header(header::AUTHORIZATION, "Bearer wrong")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(app, request).await.status(), StatusCode::UNAUTHORIZED);
    // gRPC: a call, one with a rejected credential, and a method the node does not serve.
    assert_eq!(grpc_call(app, FIND_MISSING, None).await, "0");
    assert_eq!(
        grpc_call(app, FIND_MISSING, Some("Bearer wrong")).await,
        "16"
    );
    assert_eq!(
        grpc_call(
            app,
            "/build.bazel.remote.execution.v2.Execution/Execute",
            None
        )
        .await,
        "12"
    );
    // Maven and other clients.
    let request = Request::get("/maven/v1/org/example/lib/1.0/lib.jar")
        .header(header::USER_AGENT, MAVEN_AGENT)
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(app, request).await.status(), StatusCode::NOT_FOUND);
    let request = Request::get("/readyz").body(Body::empty()).unwrap();
    assert_eq!(send(app, request).await.status(), StatusCode::OK);
    // Let the background uploads and promotions finish.
    background_settled(app, before).await;
}

/// Requests in [`workload`]: by Gradle, `PUT`s, entries stored, hits, and misses (the Maven lookup
/// misses too).
const GRADLE_REQUESTS: i64 = 10;
const PUTS: i64 = 5;
const STORED: i64 = 4;
const HITS: i64 = 4;
const GRADLE_MISSES: i64 = 1;
const MISSES: i64 = GRADLE_MISSES + 1;

/// Scrapes until the background work is done: the upload queue is empty, every store in the
/// workload was uploaded, and the disk hit was promoted. Both run in spawned tasks that finish
/// after the response.
async fn background_settled(app: &Router, before: &Scrape) {
    for _ in 0..200 {
        let now = scrape(app).await;
        let delta = |name: &str, labels: &[(&str, &str)]| {
            now.count(name, labels) - before.count(name, labels)
        };
        if now.count("mammoth_bucket_upload_queue_entries", &[]) == 0
            && delta("mammoth_bucket_uploads_total", &[("result", "ok")]) >= STORED
            && delta(
                "mammoth_tier_promotions_total",
                &[("from", "disk"), ("to", "memory")],
            ) >= 1
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("background uploads and promotions did not settle");
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn exposition_matches_the_catalog() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let app = three_tier_app(dir.path(), authenticator()).await;
    let before = scrape(&app).await;
    workload(&app, &before).await;
    let scrape = scrape(&app).await;

    let mut failures = Vec::new();
    let families: BTreeSet<String> = scrape
        .samples
        .iter()
        .map(|(n, _, _)| scrape.family(n))
        .chain(scrape.types.keys().cloned())
        .collect();

    // Every `mammoth_` family is catalogued.
    for family in families.iter().filter(|f| f.starts_with("mammoth_")) {
        let Some(metric) = catalog(family) else {
            failures.push(format!("{family}: exported but not in the 0004 catalog"));
            continue;
        };
        if !metric.live {
            failures.push(format!("{family}: exported; flip its CATALOG row to live"));
        }
    }

    for metric in CATALOG {
        let exported = families.contains(metric.name);
        if !exported {
            if metric.live {
                failures.push(format!(
                    "{}: live in the catalog but not exported",
                    metric.name
                ));
            }
            continue;
        }
        // Right `# TYPE`.
        match scrape.types.get(metric.name) {
            Some(kind) if kind == metric.kind.prometheus_type() => {}
            other => failures.push(format!(
                "{}: # TYPE is {other:?}, catalog says {}",
                metric.name,
                metric.kind.prometheus_type()
            )),
        }
        // Exactly the catalogued label keys, and bounded values within their sets.
        let want: BTreeSet<&str> = metric.labels.iter().map(|l| l.0).collect();
        for (sample, labels, _) in scrape.samples_of(metric.name) {
            let mut keys: BTreeSet<&str> = labels.keys().map(String::as_str).collect();
            if sample.ends_with("_bucket") {
                keys.remove("le");
            }
            if keys != want {
                failures.push(format!("{sample}: labels {keys:?}, catalog says {want:?}"));
            }
            for Label(key, allowed) in metric.labels {
                let Some(value) = labels.get(*key) else {
                    continue;
                };
                if let Some(allowed) = allowed
                    && !allowed.contains(&value.as_str())
                {
                    failures.push(format!("{sample}: {key}={value:?} not in {allowed:?}"));
                }
                if *key == "route" && !value.contains('{') && !value.starts_with('/') {
                    failures.push(format!("{sample}: route={value:?} is not a template"));
                }
                if *key == "route"
                    && value.len() == 32
                    && value.bytes().all(|b| b.is_ascii_hexdigit())
                {
                    failures.push(format!("{sample}: route={value:?} looks like a cache key"));
                }
            }
        }
        // Histograms expose buckets, a sum and a count.
        if metric.kind == Kind::Histogram {
            for suffix in ["_bucket", "_sum", "_count"] {
                let name = format!("{}{suffix}", metric.name);
                if !scrape.samples.iter().any(|(n, _, _)| *n == name) {
                    failures.push(format!("{name}: missing"));
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "catalog drift:\n  {}",
        failures.join("\n  ")
    );
}

#[tokio::test]
async fn workload_moves_the_live_counters() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let app = three_tier_app(dir.path(), authenticator()).await;
    let before = scrape(&app).await;
    workload(&app, &before).await;
    let after = scrape(&app).await;
    let delta = |name: &str, labels: &[(&str, &str)]| {
        after.count(name, labels) - before.count(name, labels)
    };

    // Requests, by route template and client.
    let requests = |labels: &[(&str, &str)]| delta("mammoth_http_requests_total", labels);
    assert_eq!(
        requests(&[
            ("route", "/cache/{key}"),
            ("client", "gradle"),
            ("code", "404")
        ]),
        GRADLE_MISSES
    );
    assert_eq!(
        requests(&[
            ("route", "/cache/{key}"),
            ("client", "gradle"),
            ("code", "200"),
            ("method", "GET")
        ]),
        HITS
    );
    assert_eq!(
        requests(&[
            ("route", "/cache/{key}"),
            ("client", "gradle"),
            ("method", "PUT")
        ]),
        PUTS
    );
    assert_eq!(requests(&[("client", "maven")]), 1);
    assert_eq!(requests(&[("route", "/readyz"), ("client", "other")]), 1);
    assert_eq!(requests(&[("code", "401")]), 1);
    assert_eq!(
        delta(
            "mammoth_gradle_requests_total",
            &[("gradle_version", "8.10")]
        ),
        GRADLE_REQUESTS
    );
    // The memory tier evicted, and the evicted entry's disk hit was promoted back.
    assert!(
        delta(
            "mammoth_tier_evictions_total",
            &[("tier", "memory"), ("reason", "size")]
        ) >= 1
    );
    assert!(
        delta(
            "mammoth_tier_promotions_total",
            &[("from", "disk"), ("to", "memory")]
        ) >= 1
    );
    // Every stored entry reached the bucket; the oversized one never queued.
    assert_eq!(
        delta("mammoth_bucket_uploads_total", &[("result", "ok")]),
        STORED
    );
    assert_eq!(
        delta("mammoth_bucket_uploads_total", &[("result", "error")]),
        0
    );
    assert_eq!(after.count("mammoth_bucket_upload_queue_entries", &[]), 0);
    // Every request timed, none left in flight, and the rejected credential counted.
    assert_eq!(
        delta(
            "mammoth_http_request_duration_seconds_count",
            &[("route", "/cache/{key}"), ("method", "PUT")]
        ),
        PUTS
    );
    assert_eq!(
        after.count(
            "mammoth_http_requests_in_flight",
            &[("route", "/cache/{key}")]
        ),
        0
    );
    // One rejected credential on each port.
    assert_eq!(
        delta(
            "mammoth_auth_failures_total",
            &[("reason", "CREDENTIALS_INVALID")]
        ),
        2
    );
    grpc_counted(&delta, &after);
    assert_cache_outcomes(&delta);
    tier_counters_moved(&before, &after);
    tier_population(&after);
}

/// How much a series moved across [`workload`], by name and labels.
type Delta<'a> = dyn Fn(&str, &[(&str, &str)]) -> i64 + 'a;

/// The gRPC calls in [`workload`], counted outside authentication: the refused one by its code,
/// and the unserved method as `other`.
fn grpc_counted(delta: &Delta<'_>, after: &Scrape) {
    let calls = |method, code| {
        delta(
            "mammoth_grpc_requests_total",
            &[("method", method), ("code", code)],
        )
    };
    assert_eq!(calls("FindMissingBlobs", "OK"), 1);
    assert_eq!(calls("FindMissingBlobs", "UNAUTHENTICATED"), 1);
    assert_eq!(calls("other", "UNIMPLEMENTED"), 1);
    assert_eq!(
        delta(
            "mammoth_grpc_request_duration_seconds_count",
            &[("method", "FindMissingBlobs")]
        ),
        2
    );
    assert_eq!(
        after.count(
            "mammoth_grpc_requests_in_flight",
            &[("method", "FindMissingBlobs")]
        ),
        0
    );
}

/// The cache outcome counters after [`workload`]: store results, entry sizes and
/// lookups by tier.
fn assert_cache_outcomes(delta: &Delta<'_>) {
    // Store outcomes: four written, one oversized, and a size for each one written.
    let stores = |result| delta("mammoth_cache_stores_total", &[("result", result)]);
    assert_eq!(stores("stored"), STORED);
    assert_eq!(stores("too_large"), PUTS - STORED);
    assert_eq!(stores("error"), 0);
    assert_eq!(delta("mammoth_cache_entry_size_bytes_count", &[]), STORED);
    // Hits by the tier that served them, misses with no tier.
    assert_eq!(
        delta("mammoth_cache_lookups_total", &[("result", "hit")]),
        HITS
    );
    assert!(
        delta(
            "mammoth_cache_lookups_total",
            &[("result", "hit"), ("tier", "disk")]
        ) >= 1
    );
    assert_eq!(
        delta(
            "mammoth_cache_lookups_total",
            &[("result", "miss"), ("tier", "")]
        ),
        MISSES
    );
}

/// The population gauges after [`workload`], as of the scrape: the disk holds every stored
/// entry.
fn tier_population(after: &Scrape) {
    let population = |name, tier| after.count(name, &[("tier", tier)]);
    assert_eq!(population("mammoth_tier_entries", "disk"), STORED);
    assert_eq!(population("mammoth_tier_capacity_bytes", "disk"), 1 << 20);
    assert!(
        population("mammoth_tier_size_bytes", "disk") >= 64 + 3 * i64::try_from(ENTRY).unwrap()
    );
    assert_eq!(
        population("mammoth_tier_capacity_bytes", "memory"),
        i64::try_from(MEMORY_CAPACITY).unwrap()
    );
    assert!(population("mammoth_tier_entries", "memory") >= 1);
}

/// The per-tier counters after [`workload`]: every stored entry written to the disk and the
/// bucket, each write timed, promotions written to memory, and no tier failing.
fn tier_counters_moved(before: &Scrape, after: &Scrape) {
    let delta = |name: &str, labels: &[(&str, &str)]| {
        after.count(name, labels) - before.count(name, labels)
    };
    let stored_bytes = 64 + 3 * i64::try_from(ENTRY).unwrap();
    for tier in ["disk", "bucket"] {
        let writes = |result| {
            delta(
                "mammoth_tier_writes_total",
                &[("tier", tier), ("result", result)],
            )
        };
        assert_eq!(writes("ok"), STORED, "{tier}");
        assert_eq!(
            delta("mammoth_tier_written_bytes_total", &[("tier", tier)]),
            stored_bytes,
            "{tier}"
        );
        assert_eq!(
            delta(
                "mammoth_tier_write_duration_seconds_count",
                &[("tier", tier)]
            ),
            STORED,
            "{tier}"
        );
    }
    for tier in ["memory", "disk", "bucket"] {
        for metric in ["mammoth_tier_writes_total", "mammoth_tier_reads_total"] {
            assert_eq!(
                delta(metric, &[("tier", tier), ("result", "error")]),
                0,
                "{metric} {tier}"
            );
        }
    }
    // Promotions are writes to memory too.
    assert!(
        delta(
            "mammoth_tier_writes_total",
            &[("tier", "memory"), ("result", "ok")]
        ) > STORED
    );
    // The disk hit's bytes were read from the disk, and every read was timed.
    assert!(
        delta("mammoth_tier_read_bytes_total", &[("tier", "disk")])
            >= i64::try_from(ENTRY).unwrap()
    );
    for tier in ["memory", "disk"] {
        assert_eq!(
            delta(
                "mammoth_tier_read_duration_seconds_count",
                &[("tier", tier)]
            ),
            delta("mammoth_tier_reads_total", &[("tier", tier)]),
            "{tier}"
        );
    }
}

#[tokio::test]
async fn an_aborted_upload_is_not_a_tier_failure() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let app = three_tier_app(dir.path(), authenticator()).await;
    let before = scrape(&app).await;
    // The client sends half the entry, then its connection fails.
    let body = futures_util::stream::iter([
        Ok(vec![0xAB; 512]),
        Err(std::io::Error::other("connection reset")),
    ]);
    let mut request = gradle_put("b4f5e1c09b2d4e7f8a1b6c3d5e9f0a21", 1024);
    *request.body_mut() = Body::from_stream(body);
    assert_eq!(send(&app, request).await.status(), StatusCode::BAD_REQUEST);
    let after = scrape(&app).await;
    for tier in ["memory", "disk", "bucket"] {
        let labels = [("tier", tier), ("result", "error")];
        assert_eq!(
            after.count("mammoth_tier_writes_total", &labels)
                - before.count("mammoth_tier_writes_total", &labels),
            0,
            "{tier}"
        );
    }
}

#[tokio::test]
async fn server_status_agrees_with_the_counters() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let app = three_tier_app(dir.path(), authenticator()).await;
    let before = scrape(&app).await;
    workload(&app, &before).await;
    let after = scrape(&app).await;

    let response = send(
        &app,
        Request::get("/v1/serverStatus")
            .header(header::AUTHORIZATION, TOKEN)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let status: Value = serde_json::from_str(&text(response).await).unwrap();
    let count = |field: &str| status[field].as_str().unwrap().parse::<i64>().unwrap();
    assert_eq!((count("hitCount"), count("missCount")), (HITS, MISSES));

    // Until `mammoth_cache_lookups_total` is live, serverStatus is the only source of these.
    if catalog("mammoth_cache_lookups_total").unwrap().live {
        let lookups = |result| {
            after.count("mammoth_cache_lookups_total", &[("result", result)])
                - before.count("mammoth_cache_lookups_total", &[("result", result)])
        };
        assert_eq!(
            (lookups("hit"), lookups("miss")),
            (count("hitCount"), count("missCount"))
        );
    }
}

#[tokio::test]
async fn exposition_has_help_build_info_and_process_metrics() {
    // Its miss would land in another test's counter deltas.
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let app = three_tier_app(dir.path(), authenticator()).await;
    send(&app, gradle_get("a3f5e1c09b2d4e7f8a1b6c3d5e9f0a99")).await;
    let response = send(&app, Request::get("/metrics").body(Body::empty()).unwrap()).await;
    let body = text(response).await;
    let scrape = Scrape::parse(&body);

    // Every exported `mammoth_` family carries the HELP text from 0004.
    let families: BTreeSet<String> = scrape
        .types
        .keys()
        .filter(|f| f.starts_with("mammoth_"))
        .cloned()
        .collect();
    assert!(!families.is_empty());
    for family in &families {
        assert!(
            body.contains(&format!("# HELP {family} ")),
            "{family}: no HELP line"
        );
    }

    // One sample, always 1. Not compared with this test's own `CARGO_PKG_VERSION`: Bazel sets
    // that per target, and only the library gets the crate's version.
    let info: Vec<_> = scrape.samples_of("mammoth_build_info").collect();
    assert_eq!(info.len(), 1, "{info:?}");
    let (_, labels, value) = info[0];
    assert!((*value - 1.0).abs() < f64::EPSILON, "{value}");
    assert!(
        labels.get("version").is_some_and(|v| !v.is_empty()),
        "{labels:?}"
    );
    if cfg!(target_os = "linux") {
        assert!(scrape.value("process_resident_memory_bytes", &[]) > 0.0);
    }
}

#[test]
fn docs_list_every_catalogued_metric() {
    let design = include_str!("../../../docs/design/0004-metrics.md");
    let user = include_str!("../../../docs/user/metrics.md");
    let missing: Vec<_> = CATALOG
        .iter()
        .map(|m| m.name)
        .filter(|name| !design.contains(name) || !user.contains(name))
        .collect();
    assert!(missing.is_empty(), "not in both metrics docs: {missing:?}");
}

#[test]
fn parser_reads_labels_and_histograms() {
    let scrape = Scrape::parse(
        "# TYPE mammoth_x_seconds histogram\n\
         mammoth_x_seconds_bucket{tier=\"disk\",le=\"0.1\"} 3\n\
         mammoth_x_seconds_sum{tier=\"disk\"} 0.25\n\
         mammoth_x_seconds_count{tier=\"disk\"} 3\n\
         # TYPE plain counter\n\
         plain 7\n\
         quoted{route=\"/cache/{key}\",q=\"a\\\"b\"} 1\n",
    );
    assert_eq!(
        scrape.family("mammoth_x_seconds_bucket"),
        "mammoth_x_seconds"
    );
    assert_eq!(scrape.samples_of("mammoth_x_seconds").count(), 3);
    assert_eq!(scrape.count("plain", &[]), 7);
    assert_eq!(
        scrape.count("quoted", &[("route", "/cache/{key}"), ("q", "a\"b")]),
        1
    );
}

/// An upload that breaks off on the client's side counts as `client_error`, not as a storage
/// `error`.
#[tokio::test]
async fn a_broken_upload_is_a_client_error() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let app = three_tier_app(dir.path(), authenticator()).await;
    let before = scrape(&app).await;
    let chunks: Vec<Result<Vec<u8>, std::io::Error>> = vec![
        Ok(vec![0xAB; 100]),
        Err(std::io::Error::other("connection reset")),
    ];
    let request = Request::put("/cache/a3f5e1c09b2d4e7f8a1b6c3d5e9f0b01")
        .header(header::USER_AGENT, GRADLE_AGENT)
        .header(header::CONTENT_LENGTH, 1000)
        .body(Body::from_stream(futures_util::stream::iter(chunks)))
        .unwrap();
    let response = send(&app, request).await;
    assert!(response.status().is_client_error(), "{}", response.status());
    let after = scrape(&app).await;
    let delta = |result| {
        after.count("mammoth_cache_stores_total", &[("result", result)])
            - before.count("mammoth_cache_stores_total", &[("result", result)])
    };
    assert_eq!(delta("client_error"), 1);
    assert_eq!(delta("error"), 0);
}

/// `gradle_version` comes from a header anyone can send, so it labels only Gradle requests that
/// got past authentication, and only versions in range: other requests mint no series.
#[tokio::test]
async fn gradle_versions_are_bounded() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let app = three_tier_app(dir.path(), authenticator()).await;
    let series = |scrape: &Scrape| {
        scrape
            .samples_of("mammoth_gradle_requests_total")
            .map(|(_, labels, _)| labels["gradle_version"].clone())
            .collect::<BTreeSet<_>>()
    };
    let before = scrape(&app).await;
    let get = |path: String, version: String, auth: Option<&str>| {
        let mut request = Request::get(path).header("x-gradle-version", version);
        if let Some(auth) = auth {
            request = request.header(header::AUTHORIZATION, auth);
        }
        send(&app, request.body(Body::empty()).unwrap())
    };
    for i in 0..50 {
        // Off the Gradle route, refused, and out of range.
        get("/nope".to_owned(), format!("{i}.{i}"), None).await;
        get(
            "/cache/a3f5e1c09b2d4e7f8a1b6c3d5e9f0c01".to_owned(),
            format!("{}.{i}", i % 20 + 1),
            Some("Bearer wrong"),
        )
        .await;
        get(
            "/cache/a3f5e1c09b2d4e7f8a1b6c3d5e9f0c01".to_owned(),
            format!("{}.{}", 1000 + i, i),
            None,
        )
        .await;
    }
    let after = scrape(&app).await;
    let new: BTreeSet<_> = series(&after)
        .difference(&series(&before))
        .cloned()
        .collect();
    assert!(new.iter().all(|v| v == "other"), "{new:?}");
}

/// `disk.size = "max-available"` sizes the disk tier from the volume holding `disk.path`, less
/// the free space buffer, and `/metrics` reports the resolved capacity.
#[cfg(unix)]
#[tokio::test]
async fn max_available_disk_capacity_is_the_volume_less_the_buffer() {
    let _serial = serial().await;
    let handle = recorder();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("not-yet-created");
    let file = dir.path().join("mammoth.toml");
    std::fs::write(
        &file,
        format!(
            "version = 1\n[disk]\npath = {:?}\nsize = \"max-available\"\n\
             free_space_buffer = \"1MiB\"\n",
            path.display().to_string()
        ),
    )
    .unwrap();
    let config = mammoth_cache::config::Config::load(&file).unwrap();
    let disk = config.disk.as_ref().unwrap();

    let capacity = config.resolve_disk_size(disk).unwrap().bytes();
    let stats = rustix::fs::statvfs(dir.path()).unwrap();
    assert_eq!(capacity, stats.f_blocks * stats.f_frsize - (1 << 20));

    let store = DiskStore::open(DiskConfig {
        path: disk.path.clone(),
        capacity_bytes: capacity,
        max_entry_age: None,
        free_space_buffer: disk.free_space_buffer.bytes(),
    })
    .await
    .unwrap();
    let cache = Cache::builder()
        .bounded_tier(Arc::new(MemoryStore::new(1 << 20)))
        .bounded_tier(Arc::new(store))
        .build();
    let app = router_with_auth(Arc::new(cache), Some(handle), Arc::new(authenticator()));
    assert_eq!(
        scrape(&app)
            .await
            .count("mammoth_tier_capacity_bytes", &[("tier", "disk")]),
        i64::try_from(capacity).unwrap()
    );
}
