# Quickstart

This page gets a node running on your machine and checks that it stores and serves an entry.

Without a config file, a node keeps entries in memory only (1 GiB, least useful entries evicted
first), listens on port 5071, which is the Build Cache Node's port, and allows every request.
Everything is lost when it stops. Add a [config file](configuration.md) for the disk and bucket
tiers and authentication.

## With Docker

Each release is published as `ghcr.io/nevzheng/mammoth-cache:<version>` for linux/amd64 and
linux/arm64, with `:latest` following the newest release:

```sh
docker run --rm -p 5071:5071 ghcr.io/nevzheng/mammoth-cache:0.1.0
```

The image runs as a non-root user on a distroless base, and listens on port 5071 over IPv4 and IPv6.
Release candidates are published the same way, tagged `<version>-rc.N`; `:latest` skips them.
Nothing is published from `main` between releases. For a newer build, build the image yourself
from a clone of the repository:

```sh
git clone https://github.com/nevzheng/mammoth-cache.git
cd mammoth-cache
docker build -t mammoth-cache .
docker run --rm -p 5071:5071 mammoth-cache
```

## With the release binary

Each [GitHub release](https://github.com/nevzheng/mammoth-cache/releases) carries static Linux
binaries for x86_64 and aarch64 and a macOS arm64 binary, as `mammoth-<target>.tar.gz` with the
licenses and README inside:

```sh
curl -LO https://github.com/nevzheng/mammoth-cache/releases/download/v0.1.0/mammoth-x86_64-unknown-linux-musl.tar.gz
tar -xzf mammoth-x86_64-unknown-linux-musl.tar.gz
./mammoth-x86_64-unknown-linux-musl/mammoth --version
# mammoth 0.1.0
./mammoth-x86_64-unknown-linux-musl/mammoth
```

The other targets are `aarch64-unknown-linux-musl` and `aarch64-apple-darwin`. File names carry no
version, so `releases/latest/download/<file>` always fetches the newest release.

### Verify a download

Every release ships `SHA256SUMS` and a build attestation for each tarball and for the image, signed
by GitHub for this repository's release workflow. Check both with the
[GitHub CLI](https://cli.github.com):

```sh
curl -LO https://github.com/nevzheng/mammoth-cache/releases/download/v0.1.0/SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
gh attestation verify mammoth-x86_64-unknown-linux-musl.tar.gz -R nevzheng/mammoth-cache
gh attestation verify oci://ghcr.io/nevzheng/mammoth-cache:0.1.0 -R nevzheng/mammoth-cache
```

A passing check means the file or image was built by this repository's workflow from the tagged
commit, not altered since. The image also carries an SBOM and provenance in the registry.

## From source

You need a Rust toolchain; [rustup](https://rustup.rs) picks the right version from the repository's
`rust-toolchain.toml`.

```sh
git clone https://github.com/nevzheng/mammoth-cache.git
cd mammoth-cache
cargo build --release --bin mammoth
./target/release/mammoth
```

With no config file, the binary listens on `[::]:5071` (IPv4 and IPv6) and keeps entries in memory
only. To change that, write a [config file](configuration.md) and pass it with `--config`:

```sh
./target/release/mammoth --config mammoth.toml
```

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

## Upgrading

Nodes are stateless, so an upgrade is a new image tag or binary and a restart; the disk tier is
re-indexed on start. `mammoth --version` prints the running version, and the same string is in
`/v1/serverStatus` and `mammoth_build_info`. Include it in bug reports. Before 1.0 a release may
change the config file or the disk format; the release notes say when.

## Next steps

- Point a Gradle build at the node: [Gradle](gradle.md).
- Run it behind a load balancer or in Kubernetes: [Health checks](health.md).
