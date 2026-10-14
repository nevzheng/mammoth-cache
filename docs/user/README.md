# Overview

mammoth-cache is a remote build cache node for Gradle and Maven, written in Rust. It is meant as a
drop-in replacement for the Gradle Build Cache Node, which Gradle stops distributing and supporting
after December 31, 2026. Point your builds at it the way you pointed them at the Build Cache Node,
and they reuse task and module outputs that other builds have already produced.

> [!WARNING]
> mammoth-cache is pre-alpha. The Gradle and Maven protocols work today with an in-memory cache, and
> an `edge` image is published from `main`, but there is no release yet. The disk and bucket tiers,
> the config file, authentication and most metrics are still being built. Each page says what works
> now and what is planned. Don't run it in production yet.

## What it does

A build cache node stores the outputs of build steps under keys the build tool computes from their
inputs. When a later build has the same inputs, it downloads the outputs instead of rebuilding
them. A node does not need to understand what it stores: every entry is opaque bytes under a key.

mammoth-cache keeps entries in up to three tiers:

| Tier      | Holds                                     | Status    |
| --------- | ----------------------------------------- | --------- |
| L1 memory | Small, hot entries                        | Available |
| L2 disk   | Recently used entries, one file each      | Planned   |
| L3 bucket | Every entry, in S3, GCS or Azure Blob     | Planned   |

A lookup tries memory, then disk, then the bucket, and copies a hit up into the faster tiers. With
a bucket, nodes are stateless: you can run several behind a load balancer, and a replacement node
refills from the bucket as it serves. Without one, a single node works like the Build Cache Node.

## Supported clients

| Client                                                                                                 | Versions                        | Status    |
| ------------------------------------------------------------------------------------------------------ | ------------------------------- | --------- |
| Gradle's built-in [HTTP build cache](https://docs.gradle.org/current/userguide/build_cache.html)       | 7.6 to 9.x                      | Available |
| [Apache Maven Build Cache Extension](https://maven.apache.org/extensions/maven-build-cache-extension/) | 1.2.0 and later, on Maven 3.9.x | Available |

Develocity's own Gradle plugin connector and Maven extension are not supported. Bazel remote
caching is out of scope; [bazel-remote](https://github.com/buchgr/bazel-remote) already serves it
well.

## Feature status

| Feature                                                 | Status    | Page                                 |
| ------------------------------------------------------- | --------- | ------------------------------------ |
| Gradle `GET`, `HEAD`, `PUT` on `/cache/{key}`           | Available | [Gradle](gradle.md)                  |
| Entry size limit, oversized stores accepted and dropped | Available | [Gradle](gradle.md#entry-size-limit) |
| In-memory tier (1 GiB, fixed for now)                   | Available | [Configuration](configuration.md)    |
| `/ping`, `/livez`, `/readyz`                            | Available | [Health checks](health.md)           |
| Admin status, `/v1/serverStatus`                        | Available | [Health checks](health.md)           |
| Maven routes, `/maven/{path}`                           | Available | [Maven](maven.md)                    |
| `mammoth.toml` config file                              | Planned   | [Configuration](configuration.md)    |
| Disk and bucket tiers                                   | Planned   | [Configuration](configuration.md)    |
| Users, tokens and scopes                                | Planned   | [Authentication](authentication.md)  |
| TLS                                                     | Planned   | [Configuration](configuration.md)    |
| Request metrics at `/metrics`                           | Available | [Metrics](metrics.md)                |
| `edge` container image from `main`                      | Available | [Quickstart](quickstart.md)          |
| Full metrics catalog                                    | Planned   | [Metrics](metrics.md)                |
| Release binaries and versioned images                   | Planned   | [Quickstart](quickstart.md)          |

## Where to go next

- [Quickstart](quickstart.md): run a node with Docker or from source.
- [Gradle](gradle.md) and [Maven](maven.md): point your builds at it.
- [Migrating from the Build Cache Node](migrating.md): what maps to what.
- [Configuration reference](configuration.md): every setting.
- [Custom builds](custom-builds.md): build your own distribution with your own storage backend.

The [design docs](../design/README.md) explain why mammoth-cache works the way it does.
