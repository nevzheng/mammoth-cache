# mammoth-cache

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="branding/logo-dark.svg">
  <img alt="mammoth-cache" src="branding/logo.svg" width="320">
</picture>

**mammoth-cache is an open-source, self-hosted remote build cache for Gradle, Maven and Bazel,
written in Rust.** It is a drop-in replacement for the Gradle Build Cache Node, which Gradle stops
distributing and supporting after December 31, 2026: same `/cache/` path, same port 5071, same
`/ping` health check, and no Develocity. One static binary, a few lines of config and a load
balancer, and your builds keep their cache.

> Early stage: there is no tagged release yet, and `main` and the `edge` image are what we test.
> Everything below marked "today" works there. Deploy at your own judgment.

## What it is

A build cache stores the outputs of build steps under keys the build tool computes from their
inputs. When a later build has the same inputs, it downloads the outputs instead of rebuilding
them. mammoth-cache is the server side of that: it holds opaque bytes under keys and hands them
back fast. It understands nothing about your projects, and it doesn't need to.

It speaks three protocols natively, today:

| Client                                                                                                 | Path       | Versions                        |
| ------------------------------------------------------------------------------------------------------ | ---------- | ------------------------------- |
| Gradle's built-in [HTTP build cache](https://docs.gradle.org/current/userguide/build_cache.html)       | `/cache/`  | Gradle 7.6 to 9.x               |
| [Apache Maven Build Cache Extension](https://maven.apache.org/extensions/maven-build-cache-extension/) | `/maven/`  | 1.2.0 and later, on Maven 3.9.x |
| Bazel's [HTTP remote cache](https://bazel.build/remote/caching#http-caching), off by default           | `/bazel/`  | SHA-256 digests                 |

## Why

Teams that ran the free Build Cache Node have two paths after 2026: pay for Develocity, or point
Gradle at a generic WebDAV or object store and lose the cache's semantics (size limits, atomic
writes, hit and miss metrics, a clean answer to "is this entry there"). mammoth-cache is the third
path: a cache node that is as easy to run as the old one, speaks Maven and Bazel too, and is built
for a few stateless nodes behind a load balancer rather than one JVM with a web UI.

- **Drop-in.** Change the host in `settings.gradle.kts` and the credential; nothing else.
  [Migration guide](docs/user/migrating.md).
- **Tiered.** Entries live in memory, then on local disk, then in an optional S3, GCS or Azure
  bucket. A lookup walks down and copies a hit back up. With a bucket, nodes are stateless and a
  replacement node refills itself as it serves.
- **Easy to operate.** One dependency, the bucket, and even that is optional. No database, no JVM,
  no web UI. TLS at the ingress by default, or on the node with two lines of config.
- **Honest about failure.** A full disk or a dead bucket costs misses, never errors: Gradle keeps
  its cache on. `SIGTERM` drains the node out of rotation before it exits.
- **Observable.** Prometheus metrics with a documented catalog, `/livez` and `/readyz`, and a
  `serverStatus` admin API.
- **Open.** MIT or Apache-2.0, every design decision written down in
  [docs/design](docs/design/README.md).

## Quickstart

```sh
docker run --rm -p 5071:5071 ghcr.io/nevzheng/mammoth-cache:edge
curl http://localhost:5071/ping   # SUCCESS
```

Then point a build at it, `http://localhost:5071/cache/` for Gradle, and watch
`curl http://localhost:5071/v1/serverStatus` count hits. Without a config file the node keeps
1 GiB in memory and allows every request. A minimal `mammoth.toml` adds disk, a bucket and tokens:

```toml
version = 1

[disk]
path = "/var/lib/mammoth"
size = "50GiB"

[bucket]
url = "s3://build-cache/mammoth"

[auth]
anonymous = ["cache:read"]

[auth.users.ci]
scopes = ["cache:read", "cache:write"]
token_hashes = ["sha256:..."]   # from `mammoth token new`
```

The [quickstart](docs/user/quickstart.md), [configuration reference](docs/user/configuration.md)
and [deployment guide](docs/user/deployment.md) take it from there.

## Status

| Area                                                   | Today                              |
| ------------------------------------------------------ | ---------------------------------- |
| Gradle, Maven and Bazel HTTP protocols                 | Served; acceptance suite in review |
| Bazel gRPC cache (REAPI ActionCache, CAS, ByteStream)  | In progress                        |
| Memory, disk and bucket tiers                          | Done                               |
| `mammoth.toml`, users, tokens and scopes               | Done                               |
| Graceful shutdown, readiness checks, timeouts          | Done                               |
| Optional built-in TLS with certificate reload          | Done                               |
| Prometheus metrics                                     | Core metrics; the rest landing     |
| Release binaries and versioned images                  | Pipeline ready; first release soon |

Details per feature are in the [user guide](docs/user/README.md#feature-status).

## Non-goals

- **No remote execution.** mammoth-cache is a cache, not a build system. The gRPC cache side of
  Bazel's Remote Execution API (Capabilities, ActionCache, CAS, ByteStream) is in progress
  ([design doc 0016](docs/design/0016-reapi-cache.md)); the Execution service never will be. For
  remote execution, use a REAPI build service.
- **No Develocity.** The Develocity Gradle plugin, Maven extension and build scans are not
  supported; use the clients' built-in HTTP caches.
- **No web UI, no runtime configuration API.** Configuration is a file; dashboards live in Grafana.
- **No replication between nodes.** The bucket is the shared state; nodes hold copies.

## Documentation

Read the docs at <https://nevzheng.github.io/mammoth-cache/>: the
[user guide](docs/user/README.md), the [philosophy](docs/philosophy.md) and the
[design docs](docs/design/README.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the
work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions. See [CONTRIBUTING.md](CONTRIBUTING.md).
