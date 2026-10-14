# Overview

mammoth-cache is a remote build cache node for Gradle, Maven and Bazel, written in Rust. It is meant
as a drop-in replacement for the Gradle Build Cache Node, which Gradle stops distributing and
supporting after December 31, 2026. Point your builds at it the way you pointed them at the Build
Cache Node, and they reuse task and module outputs that other builds have already produced.

> [!WARNING]
> mammoth-cache is pre-alpha. The Gradle, Maven and Bazel protocols work today with memory, disk
> and bucket tiers set in `mammoth.toml`, with authentication, graceful shutdown and optional TLS,
> and ship as versioned images and binaries from 0.1.0, with `edge` built from `main`. Still
> pre-alpha: no compatibility promises before 1.0. Each page says what works now and what is
> planned. Deploy at your own judgment.

## What it does

A build cache node stores the outputs of build steps under keys the build tool computes from their
inputs. When a later build has the same inputs, it downloads the outputs instead of rebuilding
them. A node does not need to understand what it stores: every entry is opaque bytes under a key.

mammoth-cache keeps entries in up to three tiers:

| Tier      | Holds                                 | Status                                 |
| --------- | ------------------------------------- | -------------------------------------- |
| L1 memory | Small, hot entries                    | Available                              |
| L2 disk   | Recently used entries, one file each  | Available                              |
| L3 bucket | Every entry, in S3, GCS or Azure Blob | Available                              |

A lookup tries memory, then disk, then the bucket, and copies a hit up into the faster tiers. With
a bucket, nodes are stateless: you can run several behind a load balancer, and a replacement node
refills from the bucket as it serves. Without one, a single node works like the Build Cache Node.

## Supported clients

| Client                                                                                                 | Versions                        | Status    |
| ------------------------------------------------------------------------------------------------------ | ------------------------------- | --------- |
| Gradle's built-in [HTTP build cache](https://docs.gradle.org/current/userguide/build_cache.html)       | 7.6 to 9.x                      | Available |
| [Apache Maven Build Cache Extension](https://maven.apache.org/extensions/maven-build-cache-extension/) | 1.2.0 and later, on Maven 3.9.x | Available |
| Bazel's [HTTP remote cache](https://bazel.build/remote/caching#http-caching), off by default           | SHA-256 digests                 | Available |
| The [REAPI gRPC cache](reapi.md) for Bazel, Buck2, Pants, sbt 2 and Please, off by default             | REAPI v2, SHA-256 digests       | Available |

Develocity's own Gradle plugin connector and Maven extension are not supported. Remote execution
is out of scope: mammoth-cache is a cache, not a remote build system.

## Feature status

| Feature                                                                              | Status    | Page                                      |
| ------------------------------------------------------------------------------------ | --------- | ----------------------------------------- |
| Gradle `GET`, `HEAD`, `PUT` on `/cache/{key}`                                        | Available | [Gradle](gradle.md)                       |
| Entry size limit, oversized stores accepted and dropped                              | Available | [Gradle](gradle.md#entry-size-limit)      |
| In-memory tier, 1 GiB with S3-FIFO eviction                                          | Available | [Configuration](configuration.md)         |
| `/ping`, `/livez`, `/readyz`                                                         | Available | [Health checks](health.md)                |
| Admin status, `/v1/serverStatus`                                                     | Available | [Health checks](health.md)                |
| Maven routes, `/maven/{path}`                                                        | Available | [Maven](maven.md)                         |
| Bazel HTTP cache, `/bazel/ac/` and `/bazel/cas/`, off by default                     | Available | [Bazel](bazel.md)                         |
| REAPI gRPC cache (Capabilities, ActionCache, CAS, ByteStream), off by default        | Available | [REAPI](reapi.md)                         |
| Turning each protocol on or off                                                      | Available | [Configuration](configuration.md)         |
| `mammoth.toml` config file                                                           | Available | [Configuration](configuration.md)         |
| Disk and bucket tiers, in the `mammoth` binary and [custom builds](custom-builds.md) | Available | [Configuration](configuration.md#disk-l2) |
| Users, tokens and scopes                                                             | Available | [Authentication](authentication.md)       |
| Optional TLS on the node, with certificate reload                                    | Available | [Deployment](deployment.md#tls)           |
| Graceful shutdown, readiness storage checks, timeouts                                | Available | [Deployment](deployment.md)               |
| Request, tier and bucket metrics at `/metrics`                                       | Available | [Metrics](metrics.md)                     |
| `edge` container image from `main`                                                   | Available | [Quickstart](quickstart.md)               |
| Metrics catalog (0004), all but `mammoth_tier_oldest_entry_age_seconds`              | Available | [Metrics](metrics.md)                     |
| JSON logs to stdout, with an access log and `trusted_proxies`                        | Available | [Logs](logs.md)                           |
| Release binaries and versioned images, with checksums and attestations               | Available | [Quickstart](quickstart.md)               |

## Where to go next

- [Quickstart](quickstart.md): run a node with Docker or from source.
- [Gradle](gradle.md), [Maven](maven.md) and [Bazel](bazel.md): point your builds at it.
- [Migrating from the Build Cache Node](migrating.md): what maps to what.
- [Configuration reference](configuration.md): every setting.
- [Deployment](deployment.md): load balancers, Kubernetes, sizing.
- [Logs](logs.md): the JSON line format and the access log.
- [Errors](errors.md): every error reason, with its code and status.
- [Custom builds](custom-builds.md): build your own distribution with your own storage backend.

The [design docs](../design/README.md) explain why mammoth-cache works the way it does.
