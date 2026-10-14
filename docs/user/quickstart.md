# Quickstart

This page gets a node running on your machine and checks that it stores and serves an entry.

Today a node keeps entries in memory only (1 GiB, least useful entries evicted first), listens on
port 5071, which is the Build Cache Node's port, and has no authentication. Everything is lost
when it stops.

## With Docker

There is no release yet, but every change to `main` publishes an `edge` image for linux/amd64 and
linux/arm64:

```sh
docker run --rm -p 5071:5071 ghcr.io/nevzheng/mammoth-cache:edge
```

The image runs as a non-root user on a distroless base, and listens on `0.0.0.0:5071`. `edge`
changes with every merge; pin a `sha-<commit>` tag if you need the same build twice.

To build the image yourself from a clone of the repository:

```sh
git clone https://github.com/nevzheng/mammoth-cache.git
cd mammoth-cache
docker build -t mammoth-cache .
docker run --rm -p 5071:5071 mammoth-cache
```

> [!NOTE]
> From the first release, each version is published as `ghcr.io/nevzheng/mammoth-cache:<version>`,
> with `:latest` following the newest release.

## From source

You need a Rust toolchain; [rustup](https://rustup.rs) picks the right version from the repository's
`rust-toolchain.toml`.

```sh
git clone https://github.com/nevzheng/mammoth-cache.git
cd mammoth-cache
cargo build --release --bin mammoth
./target/release/mammoth
```

With no config file, the binary listens on `0.0.0.0:5071` and keeps entries in memory only. To
change that, write a [config file](configuration.md) and pass it with `--config`:

```sh
./target/release/mammoth --config mammoth.toml
```

> [!NOTE]
> From the first release, each GitHub release carries static Linux binaries for x86_64 and
> aarch64 and a macOS arm64 binary, with `SHA256SUMS` and build attestations you can check with
> `gh attestation verify <file> -R nevzheng/mammoth-cache`.

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
