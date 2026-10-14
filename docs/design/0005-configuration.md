# 0005: Configuration

## Status

Draft

## Summary

mammoth-cache reads one static TOML file at startup and never writes to it. The file covers the
listener, TLS, the three storage tiers, users and logging. Settings can't be changed at runtime:
to change one, edit the file and restart the node, which is cheap because nodes are stateless. The
file's shape starts from the Build Cache Node's `config.yaml` and adds only what the memory and
bucket tiers need.

## Context and scope

The Build Cache Node (BCN) 21.2 reads `«data-dir»/conf/config.yaml` (schema version 5) at startup.
It also rewrites that file when settings change in its web UI or in Develocity, and it keeps
ports, TLS and the data directory in command-line flags. Its keys, from the
[BCN 21.2 manual](https://docs.develocity.ai/bcn/21.2/):

| BCN key                              | Default      | Meaning                                            |
| ------------------------------------ | ------------ | -------------------------------------------------- |
| `cache.targetSize`                   | fixed 10 GiB | Cache size, or `maxAvailable` for the whole volume |
| `cache.freeSpaceBufferSize`          | 1024 MiB     | Free disk space to keep                            |
| `cache.maxArtifactSize`              | 100 MiB      | Largest entry accepted                             |
| `cache.maxEntryAgeInHours`           | none         | Evict entries older than this                      |
| `cache.accessControl.anonymousLevel` | `none`       | `none`, `read` or `readwrite`                      |
| `cache.accessControl.users`          | none         | Name, salted password hash, `read` or `readwrite`  |
| `uiAccess`                           | `generated`  | Web UI login                                       |
| `registration`                       | none         | Connection to Develocity                           |

This doc covers the file's shape and rules. Auth is in [0002](0002-authentication.md), tiers in
[0003](0003-architecture.md).

## Goals and non-goals

### Goals

- One file is the whole configuration. Read once, never rewritten.
- Familiar to BCN users: same concepts, same defaults where they apply.
- Human units (`10GiB`, `30d`), not bare MiB or hours.
- Validated before use, and documented key by key.
- No secrets in the file.

### Non-goals

- Changing settings at runtime, through an API, a UI or hot reload.
- Overriding individual settings with environment variables or flags. One source is simpler to
  reason about.
- Develocity registration and replication settings.

## Design

### Example

```toml
# mammoth.toml
version = 1
listen = "0.0.0.0:5071"

[protocols] # which client protocols get routes (0012)
gradle = true
maven = true
bazel = false

[tls] # optional; omit to serve plain HTTP, e.g. behind a TLS-terminating proxy
cert = "/etc/mammoth/tls.crt"
key = "/etc/mammoth/tls.key"

[server] # connection timeouts and shutdown (doc 0013)
shutdown_delay = "5s" # keep serving after SIGTERM while load balancers notice
shutdown_timeout = "25s" # finish requests and queued uploads within this
idle_timeout = "75s" # above the load balancer's idle timeout
body_read_timeout = "60s" # per read, not in total

[limits]
max_entry_size = "100MiB" # BCN cache.maxArtifactSize

[memory] # L1
size = "1GiB"
max_entry_size = "1MiB" # larger entries skip memory
eviction = "s3-fifo" # or lru, w-tinylfu, sieve, fifo (0010)

[disk] # L2
path = "/var/lib/mammoth"
size = "50GiB"
free_space_buffer = "1GiB" # BCN cache.freeSpaceBufferSize
max_entry_age = "30d" # optional; BCN cache.maxEntryAgeInHours

[bucket] # L3, optional
url = "s3://acme-build-cache/mammoth/" # also gs://, az://, file://
region = "us-east-1"
# endpoint = "https://<account>.r2.cloudflarestorage.com" # S3-compatible stores only
# allow_http = true # for an http:// endpoint such as a local MinIO
write = "async" # or "sync": answer a PUT only after the upload
upload_queue = 1000 # async only
timeout = "2s" # longest bucket read, retries included; a slower one is a miss
retries = 1 # retries of a failed bucket request, within timeout
cooldown = "30s" # after a bucket failure, skip the bucket this long; "0s" turns it off

[auth]
anonymous = [] # BCN anonymousLevel; scopes for requests without credentials

[auth.users.ci]
note = "Continuous integration"
scopes = ["cache:read", "cache:write"]
token_hashes = ["sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"]

[auth.users.developers]
scopes = ["cache:read"]
token_hashes = ["sha256:60303ae22b998861bce3b28f33eec1be758a213c86c93c076dbe9f558c11c752"]

[metrics]
require_auth = false

[log]
level = "info" # error, warn, info, debug, trace
format = "json" # or "text"
access_log = true
```

### Rules

- **Location:** the path given by `--config`, else `MAMMOTH_CONFIG`, else `./mammoth.toml`. That is
  the only environment variable or flag that affects settings.
- **Defaults:** every key except `version` and `disk.path` is optional. Defaults follow the BCN
  where it has one (100 MiB entries, 1 GiB free-space buffer, no anonymous access).
- **Validation:** unknown keys, wrong types, unparseable units and contradictions (an L1 entry
  limit above the overall entry limit, an entry limit above `disk.size`, `shutdown_delay` not
  below `shutdown_timeout`) are errors at startup.
  `mammoth config check` runs the same validation without starting the server.
- **Reference:** every key is documented in the user guide's
  [configuration reference](../user/configuration.md) with its type, default, example and status,
  and set in its full example file. A test fails when either misses a key the parser accepts. There
  is no machine-readable schema; `mammoth config check` is the validator.
- **Protocols:** a protocol that is off has no routes, so its requests get the `404` of an unknown
  path. Gradle and Maven are on and Bazel is off by default; turning all three off is an error.
  See [0012](0012-bazel-http-cache-and-protocol-switches.md).
- **Units:** sizes use binary suffixes (`KiB`, `MiB`, `GiB`, `TiB`), durations `s`, `m`, `h`, `d`.
- **Secrets:** the file holds token hashes, which are not secrets. Bucket credentials come from the
  cloud's credential chain (see [0003](0003-architecture.md)). Nothing in the file needs to be kept
  from people who may read it.
- **Versioning:** `version` is the file's schema version. A new version is introduced only for a
  breaking change, and the server reads the previous version with a deprecation warning.
- **Changes:** edit the file and restart. On Kubernetes, the file is mounted from a Secret (see
  [0013](0013-deployment.md)) and a rollout restarts the pods.

### Migrating from the Build Cache Node

| BCN                                   | mammoth-cache                                                        |
| ------------------------------------- | -------------------------------------------------------------------- |
| `cache.targetSize` (`fixed`, `size`)  | `disk.size` (`50000` MiB becomes `"50000MiB"`)                       |
| `cache.targetSize.type: maxAvailable` | No equivalent yet: set `disk.size` to the volume less the buffer     |
| `cache.freeSpaceBufferSize`           | `disk.free_space_buffer`                                             |
| `cache.maxArtifactSize`               | `limits.max_entry_size`                                              |
| `cache.maxEntryAgeInHours`            | `disk.max_entry_age`                                                 |
| `anonymousLevel: read`/`readwrite`    | `auth.anonymous = ["cache:read"]` or `["cache:read", "cache:write"]` |
| `users.<name>.level`                  | `auth.users.<name>.scopes`                                           |
| `users.<name>.password`               | No equivalent: issue a token with `mammoth token new`                |
| `users.<name>.note`                   | `auth.users.<name>.note`                                             |
| `--port`, `PORT`                      | `listen`                                                             |
| `conf/ssl.crt`, `conf/ssl.key`        | `tls.cert`, `tls.key`                                                |
| `--data-dir`                          | `disk.path`                                                          |
| `uiAccess`, `registration`            | Not applicable                                                       |

BCN password hashes can't be converted, because mammoth-cache authenticates with generated tokens.
Each BCN user needs a new token, set in Gradle where the password was.

## Cross-cutting concerns

### Operations

The listener and storage change rarely, and a restart of a stateless node costs only its L1
contents (L2 can be re-indexed, see [0003](0003-architecture.md)). That makes hot reload not worth
its complexity. The one exception is the TLS certificate and key, which the node re-reads from
their files so a renewal needs no restart; see [0014](0014-tls.md).

## Alternatives considered

- **YAML,** like the BCN's `config.yaml`: familiar to BCN users and Kubernetes manifests, but full
  of surprises (implicit types, the "Norway problem", significant indentation). TOML is the Rust
  ecosystem's usual format and has one obvious way to write each value. The migration table covers
  the change of format.
- **BCN's file as is:** keys in bare MiB and hours, no place for the memory and bucket tiers, and
  password hashes. A translation table serves migrating users better.
- **Environment variable overrides for every key** (bazel-remote, fosterstack/cache): convenient
  in containers, but two sources mean precedence rules and drift. A mounted file works everywhere.
- **Hot reload:** see Operations.

## Open questions

- Whether to ship `mammoth config import-bcn` to translate a BCN `config.yaml`.
