# Quickstart

This page gets a node running on your machine and checks that it stores and serves an entry. There
are no published releases yet, so both ways below build from source.

Today a node keeps entries in memory only (1 GiB), listens on port 5071, which is the Build Cache
Node's port, and has no authentication. Everything is lost when it stops.

## With Docker

Build the image from a clone of the repository, then run it:

```sh
git clone https://github.com/nevzheng/mammoth-cache.git
cd mammoth-cache
docker build -t mammoth-cache .
docker run --rm -p 5071:5071 mammoth-cache
```

The image runs as a non-root user on a distroless base, and listens on `0.0.0.0:5071`.

> [!NOTE]
> Published images are planned: `ghcr.io/nevzheng/mammoth-cache:<version>` for each release, for
> linux/amd64 and linux/arm64, and an `edge` tag built from `main`. Until the first release, build
> the image yourself.

## From source

You need a Rust toolchain; [rustup](https://rustup.rs) picks the right version from the repository's
`rust-toolchain.toml`.

```sh
git clone https://github.com/nevzheng/mammoth-cache.git
cd mammoth-cache
cargo build --release --bin mammoth
./target/release/mammoth
```

The binary listens on `0.0.0.0:5071`. Set `MAMMOTH_ADDR` to change it, and `RUST_LOG` to change the
log level:

```sh
MAMMOTH_ADDR=127.0.0.1:8080 RUST_LOG=debug ./target/release/mammoth
```

These two variables are temporary. Once the [config file](configuration.md) lands, the address
moves to `listen` and the log level to `[log]`.

> [!NOTE]
> Release binaries are planned for each release: static Linux binaries for x86_64 and aarch64, and
> a macOS arm64 binary, with checksums and build attestations.

## Check it works

The node answers the Build Cache Node's health check:

```sh
curl http://localhost:5071/ping
# SUCCESS
```

Store an entry under a key and read it back. Keys are lowercase hex, as Gradle sends them:

```sh
echo 'hello' | curl -i -X PUT --data-binary @- http://localhost:5071/cache/0123456789abcdef
# HTTP/1.1 201 Created
curl http://localhost:5071/cache/0123456789abcdef
# hello
curl -i http://localhost:5071/cache/ffff
# HTTP/1.1 404 Not Found
```

Storing the same key again answers `200 OK` and writes nothing, because entries never change.

See what the node holds:

```sh
curl http://localhost:5071/v1/serverStatus
```

## Next steps

- Point a Gradle build at the node: [Gradle](gradle.md).
- Run it behind a load balancer or in Kubernetes: [Health checks](health.md).
