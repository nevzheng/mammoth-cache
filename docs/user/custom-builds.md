# Custom builds

The default `mammoth` build covers most deployments. When you need something it doesn't ship, such
as a different allocator, an optional backend or your own storage, build your own distribution
instead of forking. There are no runtime plugins: everything is chosen at compile time, so a custom
build is as fast as the default one.

> [!NOTE]
> The library's Rust API is not stable before 1.0, and the crate is not published to crates.io.
> Depend on it from git and pin a commit; tags come with the first release.

## Cargo features

Optional parts ship behind Cargo features, so the default binary stays small. Build with a
different set from a clone of the repository:

```sh
cargo build --release --bin mammoth --no-default-features --features jemalloc
```

| Feature    | Default | What it does                                   | Status    |
| ---------- | ------- | ---------------------------------------------- | --------- |
| `mimalloc` | yes     | Use mimalloc as the global allocator           | Available |
| `jemalloc` | no      | Use jemalloc instead; wins if both are enabled | Available |

The allocator matters because release binaries are static musl builds, and musl's own allocator is
slow under concurrent load. Which allocator stays the default will be decided by load tests.
Optional storage backends will be added to this table as they land.

## Your own storage backend

mammoth-cache is a library with a thin `mammoth` binary on top. Every storage tier implements one
trait, `BlobStore`: get, head and put of streamed entries. To use storage the default
build doesn't support, implement the trait in your own crate and write a small `main` that wires
it in.

```toml
# Cargo.toml of your distribution
[dependencies]
mammoth-cache = { git = "https://github.com/nevzheng/mammoth-cache", rev = "<commit>" }
axum = "0.8"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "net"] }
```

```rust
use std::{io, sync::Arc};

use mammoth_cache::{
    cache::Cache,
    key::CacheKey,
    store::{Blob, BlobStore, BoxFuture, ByteStream, PutOutcome, Tier},
};

#[derive(Debug)]
struct MyStore { /* a client for your storage */ }

impl BlobStore for MyStore {
    fn tier(&self) -> Tier {
        Tier::Bucket
    }

    fn get<'a>(&'a self, key: &'a CacheKey) -> BoxFuture<'a, io::Result<Option<Blob>>> {
        Box::pin(async move { todo!("stream the entry, or Ok(None) on a miss") })
    }

    fn head<'a>(&'a self, key: &'a CacheKey) -> BoxFuture<'a, io::Result<Option<u64>>> {
        Box::pin(async move { todo!("the entry's size, or Ok(None) on a miss") })
    }

    fn put<'a>(
        &'a self,
        key: &'a CacheKey,
        size_hint: Option<u64>,
        body: ByteStream,
    ) -> BoxFuture<'a, io::Result<PutOutcome>> {
        Box::pin(async move { todo!("write the stream; leave nothing behind if it fails") })
    }
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let cache = Cache::builder().tier(Arc::new(MyStore {})).build();
    let listener = tokio::net::TcpListener::bind("0.0.0.0:5071").await?;
    axum::serve(listener, mammoth_cache::router(Arc::new(cache), None)).await
}
```

Passing `None` for metrics makes `/metrics` answer `501`; to export them, install a
`metrics-exporter-prometheus` recorder and pass its handle, as the default
[`main.rs`](https://github.com/nevzheng/mammoth-cache/blob/main/crates/mammoth-cache/src/main.rs)
does.

Tiers are looked up in the order they are added. For now the core writes only to the first tier;
the full write path (disk first, then a background bucket upload) arrives with the bucket tier. So
until then, add your store as the first or only tier. The rules a store must follow:

- Gradle entries are immutable: when `key.namespace().is_immutable()`, a `put` for a key that
  already exists writes nothing and returns `PutOutcome::Exists`. Maven keys are not immutable, so
  a `put` for one replaces the entry.
- A `put` whose body stream fails must leave nothing behind, so a partial upload never becomes a
  hit.
- Stream bytes through; don't buffer whole entries.
- A local tier with a capacity also implements `BoundedStore`, so its population shows up in
  `/v1/serverStatus` and the metrics.

The public traits are the extension surface. From 1.0, changing them is a breaking change under
semver. Design doc [0006](../design/0006-server-framework.md) explains the choice.
