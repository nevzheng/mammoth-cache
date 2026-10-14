# Migrating from the Build Cache Node

Gradle stops distributing and supporting the Build Cache Node (BCN) after December 31, 2026, and
points users at Develocity Edge. mammoth-cache is an open-source alternative for teams that want a
self-hosted node without Develocity.

> [!WARNING]
> mammoth-cache is pre-alpha. Today it can stand in for a BCN in a trial (Gradle, Maven and Bazel's
> HTTP cache, with memory, disk and bucket tiers, authentication and TLS), but not in production:
> there is no release yet. This page describes the full migration as designed, so you can plan it.

## What stays the same

- **Gradle builds.** The cache lives at `/cache/` on port 5071, as on the BCN. Change the host in
  `url` and the password; nothing else in `settings.gradle.kts`.
- **Health check.** `GET /ping` answers `SUCCESS`.
- **Defaults.** 100 MiB entry limit, 1 GiB free-space buffer, no anonymous access.
- **Access levels.** BCN's `read`, `readwrite` and anonymous access map onto scopes.
- **Expect-Continue.** Supported, so oversized uploads are turned down before they are sent.

## What changes

| Build Cache Node                         | mammoth-cache                                                                      |
| ---------------------------------------- | ---------------------------------------------------------------------------------- |
| `config.yaml`, rewritten by the web UI   | `mammoth.toml`, read once, never rewritten                                         |
| Web UI and Develocity registration       | None; edit the file and restart                                                    |
| Users with passwords                     | Users with generated tokens, stored as hashes                                      |
| Replication between nodes, by Develocity | A shared bucket; nodes are stateless                                               |
| One disk per node                        | Memory, then disk, then an optional bucket                                         |
| JVM                                      | A single static binary or a distroless image                                       |
| Bazel gRPC on port 6011                  | The [REAPI gRPC cache](reapi.md) on port 6011, off by default; no remote execution |
| Develocity plugin and Maven extension    | Not supported; use Gradle's `HttpBuildCache` or the Apache Maven extension         |
| No Prometheus metrics                    | `/metrics` with a documented catalog                                               |

## Translating `config.yaml`

| BCN                                   | mammoth-cache                                                        |
| ------------------------------------- | -------------------------------------------------------------------- |
| `cache.targetSize` (`fixed`, `size`)  | `disk.size` (`50000` MiB becomes `"50000MiB"`)                       |
| `cache.targetSize.type: maxAvailable` | Not supported yet: set `disk.size` to the volume less the buffer     |
| `cache.freeSpaceBufferSize`           | `disk.free_space_buffer`                                             |
| `cache.maxArtifactSize`               | `limits.max_entry_size`                                              |
| `cache.maxEntryAgeInHours`            | `disk.max_entry_age` (`72` becomes `"72h"`)                          |
| `anonymousLevel: read`/`readwrite`    | `auth.anonymous = ["cache:read"]` or `["cache:read", "cache:write"]` |
| `users.<name>.level`                  | `auth.users.<name>.scopes`                                           |
| `users.<name>.password`               | No equivalent: issue a token with `mammoth token new`                |
| `users.<name>.note`                   | `auth.users.<name>.note`                                             |
| `--port`, `PORT`                      | `listen`                                                             |
| `conf/ssl.crt`, `conf/ssl.key`        | `tls.cert`, `tls.key`                                                |
| `--data-dir`                          | `disk.path`                                                          |
| `uiAccess`, `registration`            | Not applicable                                                       |

For example, this BCN config:

```yaml
version: 5
cache:
  targetSize:
    type: fixed
    size: 50000
  maxArtifactSize: 100
  accessControl:
    anonymousLevel: read
    users:
      ci:
        password: "..."
        level: readwrite
```

becomes:

```toml
version = 1
listen = "[::]:5071"

[limits]
max_entry_size = "100MiB"

[disk]
path = "/var/lib/mammoth"
size = "50000MiB"

[auth]
anonymous = ["cache:read"]

[auth.users.ci]
scopes = ["cache:read", "cache:write"]
token_hashes = ["sha256:..."] # from mammoth token new
```

BCN password hashes can't be converted. Give each user a new token and put it in Gradle where the
password was.

## Steps

1. Translate `config.yaml` into `mammoth.toml` with the table above, and run
   `mammoth config check`.
2. Create a token for each user with `mammoth token new`, and store each token where your builds
   read the BCN password today, such as a CI secret.
3. Start mammoth-cache next to the BCN. Health checks go to `/ping` or, better, `/livez` and
   `/readyz` (see [Health checks](health.md)).
4. Point CI, the builds that push, at the new node first, so it fills up. Then move developers'
   builds.
5. Watch the hit rate ([Metrics](metrics.md)) until it matches what the BCN gave you, then retire
   the BCN.

There is no tool to copy entries out of the BCN. A cold cache refills within a few CI builds, and
running both nodes side by side for a while covers the gap.
