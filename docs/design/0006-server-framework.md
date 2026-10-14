# 0006: Server framework and code structure

## Status

Draft

## Summary

mammoth-cache is built on axum, over hyper 1.x and the tokio runtime, with tower middleware. The
crate is a library with a thin `mammoth` binary. The code follows the ports and adapters structure
from [0003](0003-architecture.md): storage tiers implement one blob store trait, a core applies the
tiering rules, and HTTP handlers translate between the protocol and the core. The bucket tier will
use the `object_store` crate rather than each cloud's SDK. Extensions are compile-time: a custom
build implements the blob store trait in its own crate, with no fork and no runtime plugins.

## Context and scope

The server's hot path is `GET` and `PUT` on `/cache/{key}`: streamed blobs of up to 100 MiB, many
concurrent clients, and a priority on hit rate, then tail latency, then throughput (0003). The admin
API is defined in protobuf and served as HTTP/JSON with AIP-193 errors
([0001](0001-api-contracts.md)). The build uses Bazel with rules_rust, with Cargo as the source of
dependencies.

This doc covers the framework, the code layout, the storage entry format and how others extend the
server. Storage behaviour is in 0003, auth in [0002](0002-authentication.md).

## Goals and non-goals

### Goals

- Stream entries end to end with backpressure; never buffer a whole entry outside the memory tier.
- Server overhead small enough to disappear behind disk and network time.
- Shared middleware for auth, tracing, timeouts and metrics across every route.
- A framework and dependencies likely to be maintained for as long as the project is.
- Let others add a storage backend without forking.

### Non-goals

- Runtime-loaded plugins (shared libraries, WASM).
- A thread-per-core runtime, for now (see [Alternatives considered](#alternatives-considered)).

## Design

### Framework

| Concern         | Choice                                                                 |
| --------------- | ---------------------------------------------------------------------- |
| Runtime         | tokio, multi-threaded                                                  |
| HTTP            | hyper 1.x through axum                                                 |
| Middleware      | tower and tower-http (tracing, timeouts, limits, auth)                 |
| Admin API types | Hand-written serde types for now; prost and pbjson once the API grows  |
| Errors          | One AIP-193 error type, used by every route                            |
| Bucket (L3)     | `object_store` crate                                                   |
| Observability   | tracing, and a Prometheus exporter for the 0004 catalog                |

axum is a thin layer over hyper. Request and response bodies stay `http_body::Body` streams, so a
`PUT` flows into a store with backpressure, and a `GET` streams from memory, a file or the bucket.
Every route is a tower service, so tower-http middleware applies everywhere, and a route can drop
to a hand-written hyper service if a benchmark ever shows axum on the hot path.

### Code layout

```text
crates/mammoth-cache/src/
├── main.rs          thin binary: logging, listener, wiring
├── lib.rs
├── key.rs           CacheKey: validated lowercase hex, at most 128 characters
├── cache.rs         core: lookup order, size limit, hit and miss counts
├── store/
│   ├── mod.rs       ports: BlobStore, BoundedStore, Blob, ByteStream, Tier
│   └── memory.rs    L1 adapter
└── http/
    ├── mod.rs       router
    ├── error.rs     AIP-193 responses
    ├── gradle.rs    GET, HEAD and PUT /cache/{key}
    ├── ops.rs       /ping, /livez, /readyz, /metrics
    └── admin.rs     /v1/serverStatus
```

The core depends only on the ports. HTTP depends on the core. Nothing but adapters touches a file
system or a cloud.

### Ports

```rust
pub trait BlobStore: Send + Sync + Debug {
    fn tier(&self) -> Tier;
    fn get<'a>(&'a self, key: &'a CacheKey) -> BoxFuture<'a, io::Result<Option<Blob>>>;
    fn head<'a>(&'a self, key: &'a CacheKey) -> BoxFuture<'a, io::Result<Option<u64>>>;
    fn put<'a>(&'a self, key: &'a CacheKey, size_hint: Option<u64>, body: ByteStream)
        -> BoxFuture<'a, io::Result<PutOutcome>>;
}

pub trait BoundedStore: BlobStore {
    fn usage(&self) -> TierUsage;
}
```

- `ByteStream` is a boxed stream of `Bytes` chunks. Stores never see HTTP types.
- Methods return boxed futures so the core can hold tiers as `Arc<dyn BlobStore>`. One allocation
  per call is noise next to a disk or network read.
- `put` must leave nothing behind when its stream fails, so a partial upload never becomes a hit.
  The core relies on this to enforce the entry size limit while streaming.

### Hot path rules

1. `/cache/{key}` reads the raw body stream, never a buffering extractor.
2. A `Content-Length` over the limit gets `413` before the body is read. hyper sends
   `100 Continue` only when the body is first read, so a rejected upload is never transmitted.
3. The size limit is also enforced on the stream, for bodies without `Content-Length`.
4. A memory hit is a reference-counted `Bytes`, with no copy.
5. Disk and bucket I/O never block a tokio worker thread.

### Entry format

Every tier stores the same thing: a key mapping to immutable bytes and a little metadata.

| Field    | Meaning                                               |
| -------- | ----------------------------------------------------- |
| key      | Gradle's cache key                                    |
| size     | Length of the entry in bytes                          |
| checksum | Checksum of the bytes, checked on read (0003)         |
| created  | When the entry was first stored, for age-based expiry |

On disk and in the bucket an entry is one object holding a small fixed header (format version,
checksum algorithm, size), then the bytes, then the checksum as a trailer. A trailer lets a writer
compute the checksum while streaming, and one self-describing format serves both tiers, so an L3
hit can be written to L2 as is. Bucket objects live at `<prefix>/v1/<key>`; the `v1` changes only
if the format does.

### Bucket library

The bucket adapter uses the `object_store` crate (Apache Arrow) instead of the AWS, Google and Azure
SDKs:

- One interface for S3 and S3-compatible stores, GCS, Azure Blob and the local file system.
- Each cloud's default credential chain, including workload identity (see 0003).
- Streaming and multipart uploads.
- A much smaller dependency tree than three SDKs, which matters for build time and for vendoring
  under Bazel.

Apache OpenDAL is the alternative if `object_store` lacks a backend or a credential flow. Because
the bucket sits behind `BlobStore`, a native-SDK adapter can be added later without touching the
core.

### Extensions

Rust has no stable ABI, so runtime plugins mean either a C ABI or WASM, each with a cost on every
call and a large maintenance burden. Instead:

- The crate is a library and `mammoth` is a few lines of wiring. A custom build depends on the
  library, implements `BlobStore` in its own crate and wires it in its own `main`.
- Optional backends ship behind Cargo features, so the default binary stays small.
- The public traits are the extension surface. Changes to them are breaking changes and follow
  semver once the project reaches 1.0.

## Alternatives considered

- **actix-web:** fast, and its per-core runtimes help tail latency, but it has its own service model
  rather than tower, so middleware and any future gRPC would not share plumbing. Fewer
  maintainers than axum.
- **Raw hyper:** most control, but routing, extraction and errors would be ours to maintain. Kept as
  an option for individual hot routes.
- **Thread-per-core runtimes with io_uring (monoio, glommio, compio):** the best theoretical tail
  latency, but Linux-only, and the object-store, cloud and tower ecosystem assumes tokio. To be
  revisited for the disk tier alone, behind `BlobStore`, if disk-read p99 is a measured problem.
- **Pingora:** built for proxies, not origin servers, with a heavier dependency tree.
- **Rocket, Poem, Salvo:** no advantage over axum for this server, and smaller communities.
- **Cloud SDKs directly:** three large dependency trees and three code paths for the same three
  operations.
- **Runtime plugins (`dlopen`, WASM):** see [Extensions](#extensions).

## Open questions

- The checksum algorithm: CRC32C (hardware-accelerated, what the clouds use) or a stronger hash
  such as BLAKE3.
- When to generate admin types from the proto with prost and pbjson rather than by hand.
- Whether async trait methods (once usable with `dyn`) should replace the boxed futures.
