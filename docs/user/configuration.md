# Configuration reference

mammoth-cache reads one TOML file at startup. If no path is given and `./mammoth.toml` doesn't
exist, the node runs on the defaults with memory only, so `docker run` works without a file, but
entries are lost on restart, and entries over `memory.max_entry_size` are treated as oversized.
Every key is listed under [Keys](#keys), and the [full example](#full-example) sets them all.

## The config file

mammoth-cache reads one TOML file at startup and never writes to it. To change a setting, edit the
file and restart the node. On Kubernetes, mount the file from a Secret, since it holds token
hashes; a rollout restarts the pods.

The file is found at the path given by `--config`, else `MAMMOTH_CONFIG`, else `./mammoth.toml`.
That is the only flag or variable that affects settings: there are no per-key overrides. The one
exception is `RUST_LOG`, which replaces `log.level` when set, for debugging one module.

Every key except `version` and `disk.path` is optional. Unknown keys, wrong types, bad units and
contradictions are errors at startup. `mammoth config check` runs the same validation without
starting the server, and also checks what the file points at without contacting or writing
anything: the TLS certificate loads, `bucket.url` is a URL the bucket client accepts, and
`disk.path` (or its nearest parent that exists) is a writable directory.

### Units

- Sizes take binary suffixes: `KiB`, `MiB`, `GiB`, `TiB`, for example `"50GiB"`.
- Durations take `s`, `m`, `h` or `d`, for example `"30d"`.

### Example

A small file for one node with a disk and a bucket. The [full example](#full-example) sets every
key.

```toml
version = 1

[disk]
path = "/var/lib/mammoth"
size = "50GiB"

[bucket]
url = "s3://acme-build-cache/mammoth/"

[auth.users.ci]
scopes = ["cache:read", "cache:write"]
token_hashes = ["sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"]
```

## Keys

Each table gives a key's type, default, an example value and its status: **Available** keys work
today, **Not enforced** keys are accepted and validated but don't take effect yet, and **Planned**
keys are designed but rejected as unknown until they ship. A test fails when a key the code accepts
is missing here or from the [full example](#full-example), so this page is complete.

### Top level

| Key       | Type    | Default     | Example          | Status    | Meaning                                                                                                                 |
| --------- | ------- | ----------- | ---------------- | --------- | ----------------------------------------------------------------------------------------------------------------------- |
| `version` | integer | required    | `1`              | Available | Schema version of the file; `1`                                                                                         |
| `listen`  | string  | `[::]:5071` | `"0.0.0.0:5071"` | Available | Address and port; 5071 is the Build Cache Node's port. `[::]` takes IPv4 and IPv6, or IPv4 alone on a host without IPv6 |

### `[protocols]`

Which clients the node serves. A protocol that is off has no routes: its requests get `404`, like
any unknown path. Health checks, metrics and the admin API are always served. At least one
protocol must be on.

| Key      | Type    | Default | Example | Status    | Meaning                                                         |
| -------- | ------- | ------- | ------- | --------- | --------------------------------------------------------------- |
| `gradle` | boolean | `true`  | `false` | Available | Gradle's build cache at `/cache/`                               |
| `maven`  | boolean | `true`  | `false` | Available | The Maven Build Cache Extension at `/maven/`                    |
| `bazel`  | boolean | `false` | `true`  | Available | Bazel's HTTP cache at `/bazel/`; see [Bazel](bazel.md)          |
| `reapi`  | boolean | `false` | `true`  | Available | The REAPI gRPC cache on `[reapi] listen`; see [REAPI](reapi.md) |

### `[reapi]`

The gRPC listener for the REAPI cache, used only when `protocols.reapi` is on. It serves HTTP/2
(gRPC) on its own port, with the same `[tls]` certificate, users and scopes as the HTTP port.

| Key      | Type   | Default     | Example          | Status    | Meaning                                                                                  |
| -------- | ------ | ----------- | ---------------- | --------- | ---------------------------------------------------------------------------------------- |
| `listen` | string | `[::]:6011` | `"0.0.0.0:6011"` | Available | Address and port; 6011 is the Build Cache Node's Bazel gRPC port. `[::]` as for `listen` |

### `[server]`

How connections time out and how the node shuts down. Design doc
[0013](../design/0013-deployment.md) explains the choices.

Every key applies to both ports, the HTTP `listen` and the gRPC `[reapi] listen`, except where the
table says otherwise.

| Key                 | Type     | Default | Example          | Status    | Meaning                                                                                       |
| ------------------- | -------- | ------- | ---------------- | --------- | --------------------------------------------------------------------------------------------- |
| `shutdown_delay`    | duration | `5s`    | `"10s"`          | Available | After `SIGTERM`, keep serving while load balancers see `/readyz` fail                         |
| `shutdown_timeout`  | duration | `25s`   | `"55s"`          | Available | From `SIGTERM`, the most time to finish requests and queued bucket uploads                    |
| `idle_timeout`      | duration | `75s`   | `"620s"`         | Available | How long an HTTP connection may wait for its next request's headers; a new gRPC one, any byte |
| `body_read_timeout` | duration | `60s`   | `"2m"`           | Available | How long an upload may send nothing before it fails; per read, not in total                   |
| `send_timeout`      | duration | `60s`   | `"2m"`           | Available | How long a response may make no progress because the client stopped reading; per write        |
| `trusted_proxies`   | list     | `[]`    | `["10.0.0.0/8"]` | Available | Proxies whose `X-Forwarded-For` names the client in the access log: addresses or CIDRs        |

On `SIGTERM` or Ctrl-C the node fails `/readyz`, keeps serving for `shutdown_delay`, stops
accepting connections, finishes requests in flight on both ports, then finishes queued bucket
uploads, all within `shutdown_timeout`. Connections that have not sent a request close as the drain
starts. A second signal exits at once. Keep `shutdown_timeout` below the platform's grace period
(30 s on Kubernetes), and `shutdown_delay` below `shutdown_timeout`; the second is checked at
startup, and `shutdown_timeout`, `idle_timeout`, `body_read_timeout` and `send_timeout` must be
above zero.

With `trusted_proxies` empty, the access log shows each connection's peer address and ignores
`X-Forwarded-For`, which any client can send. List the load balancers and proxies in front of the
node, and a request through them is logged with the address they report. A CIDR with host bits set,
such as `10.0.0.1/8`, is refused at startup.

Keep `idle_timeout` above the load balancer's idle timeout (60 s on AWS ALB and nginx), or the
load balancer reuses a connection the node has just closed and returns `502`. Google Cloud load
balancers keep connections 600 s, so raise it there.

### `[tls]`

Optional. With it, the node serves HTTPS on `listen` instead of plain HTTP (TLS 1.2 and 1.3,
HTTP/1.1). Without it, serve plain HTTP behind a proxy that terminates TLS; with authentication on,
the node then logs a warning at startup. Design doc [0014](../design/0014-tls.md) explains the
choices.

| Key         | Type            | Default  | Example                      | Status    | Meaning                                                                                                       |
| ----------- | --------------- | -------- | ---------------------------- | --------- | ------------------------------------------------------------------------------------------------------------- |
| `cert`      | path            | required | `"/etc/mammoth/tls/tls.crt"` | Available | PEM certificate chain: the server certificate first, then any intermediates                                   |
| `key`       | path            | required | `"/etc/mammoth/tls/tls.key"` | Available | PEM private key, unencrypted: PKCS#8, PKCS#1 (RSA) or SEC1 (EC)                                               |
| `generate`  | bool            | `false`  | `true`                       | Available | Write a self-signed certificate and key to `cert` and `key` at startup when neither file exists               |
| `hostnames` | list of strings | `[]`     | `["cache.lan", "10.0.0.5"]`  | Available | Names and IP addresses a generated certificate is valid for; empty for the hostname, `localhost` and loopback |

Startup, and `mammoth config check`, fail if either file can't be read or the key doesn't match the
certificate. With `generate = true`, a missing pair is written instead (see
[A self-signed certificate](deployment.md#a-self-signed-certificate)); only one of the two files
existing is still an error. The node re-reads both files every minute and picks up a renewed pair
without a restart; see [TLS on the node](deployment.md#tls-on-the-node). The handshake timeout (10
s) and protocol versions are fixed, not settings.

### `[limits]`

| Key              | Type | Default  | Example    | Status    | Meaning                                                                                                    |
| ---------------- | ---- | -------- | ---------- | --------- | ---------------------------------------------------------------------------------------------------------- |
| `max_entry_size` | size | `100MiB` | `"500MiB"` | Available | Largest entry stored. Gradle stores over it are accepted and dropped; Maven and Bazel stores get `413`.    |

`limits.max_entry_size` may not be larger than `disk.size` (with `"max-available"`, than the size
it resolves to at startup). It is the Build Cache Node's
`cache.maxArtifactSize`; design doc [0007](../design/0007-size-limits-and-admission.md) explains
the limits.

### `[memory]` (L1)

| Key              | Type   | Default   | Example       | Status    | Meaning                                                                  |
| ---------------- | ------ | --------- | ------------- | --------- | ------------------------------------------------------------------------ |
| `size`           | size   | `1GiB`    | `"4GiB"`      | Available | Memory for entries                                                       |
| `max_entry_size` | size   | `1MiB`    | `"8MiB"`      | Available | Larger entries skip memory, so one big entry can't flush many small ones |
| `eviction`       | string | `s3-fifo` | `"w-tinylfu"` | Available | `s3-fifo`, `lru`, `w-tinylfu`, `sieve` or `fifo`                         |

`memory.max_entry_size` may not be larger than `limits.max_entry_size`. Design doc
[0010](../design/0010-memory-tier-and-eviction.md) explains the eviction policies. `size` counts
each entry's body, its key and about 160 bytes of bookkeeping. Set the container's memory limit
above it; see [Memory and disk sizing](deployment.md#memory-and-disk-sizing).

### `[disk]` (L2)

Optional. Without it the node keeps entries in memory only, which suits trials and tests.

| Key                 | Type                      | Default  | Example              | Status    | Meaning                                                       |
| ------------------- | ------------------------- | -------- | -------------------- | --------- | ------------------------------------------------------------- |
| `path`              | path                      | required | `"/var/lib/mammoth"` | Available | Directory for entries; each node needs its own                |
| `size`              | size or `"max-available"` | `10GiB`  | `"50GiB"`            | Available | Space for entries, counting each file in whole 4 KiB blocks   |
| `free_space_buffer` | size                      | `1GiB`   | `"5GiB"`             | Available | Free space to keep on the volume; the tier shrinks to keep it |
| `max_entry_age`     | duration                  | none     | `"30d"`              | Available | Evict entries older than this                                 |

`size = "max-available"` gives the tier the whole volume holding `path`, less
`free_space_buffer`, like the Build Cache Node's `targetSize.type: maxAvailable`. The node reads
the volume's size once at startup and logs it as `disk tier capacity`; a resized volume needs a
restart. `mammoth_tier_capacity_bytes{tier="disk"}` and `/v1/serverStatus` show the result. On a
tmpfs, the volume is the tmpfs size. Startup and `mammoth config check` fail if the volume is no
larger than the buffer, or the result is below `limits.max_entry_size`.

Entries are written to a temporary file and renamed into place, so a crash never leaves a partial
entry. Each file carries a CRC32C checksum, checked on every read; a damaged entry is treated as a
miss and removed. On startup the node indexes the files already on disk, so a restarted node
starts warm.

`disk.path` belongs to one process. The node holds a lock on `mammoth.lock` in that directory while
it runs, and a second node pointed at the same directory fails to start. On startup the node
deletes only files it wrote: temporary files under `tmp/` named by its own scheme, and damaged
entry files under `v1/`. Other files are logged and left alone, but don't point `disk.path` at a
directory that holds anything else.

Every 5 seconds the node writes and removes a probe file and reads the volume's free space. If free
space drops below `free_space_buffer`, it evicts entries until the buffer is free again, and
`/readyz` fails while it cannot. Those results are what `/readyz` reports. A write that finds the
volume full evicts entries until the tier is down to 90 % of its size, then carries on, so a full
volume costs a batch of evictions rather than failed stores.

### `[bucket]` (L3)

Optional. With a bucket, every entry is uploaded to it, and several nodes can share it. The bucket
tier is built on the `object_store` crate.

| Key            | Type     | Default              | Example                            | Status    | Meaning                                                                |
| -------------- | -------- | -------------------- | ---------------------------------- | --------- | ---------------------------------------------------------------------- |
| `url`          | string   | required             | `"s3://acme-build-cache/mammoth/"` | Available | `s3://`, `gs://`, `az://` or `file://` URL, with an optional prefix    |
| `region`       | string   | from the environment | `"us-east-1"`                      | Available | Region, for S3                                                         |
| `endpoint`     | string   | none                 | `"http://minio:9000"`              | Available | Endpoint for S3-compatible stores such as MinIO or R2                  |
| `allow_http`   | boolean  | from the environment | `true`                             | Available | Allow an `http://` endpoint, such as a local MinIO, SeaweedFS, Azurite |
| `write`        | string   | `async`              | `"sync"`                           | Available | `async` answers a store once it's on disk; `sync` waits for the upload |
| `upload_queue` | integer  | `1000`               | `5000`                             | Available | Uploads waiting at once, for `async`; more are dropped and counted     |
| `timeout`      | duration | `2s`                 | `"5s"`                             | Available | The longest a bucket read may take, retries included; then it's a miss |
| `retries`      | integer  | `1`                  | `3`                                | Available | Retries of a failed bucket request, within `timeout`; at most 10       |
| `cooldown`     | duration | `30s`                | `"0s"`                             | Available | After a bucket failure, how long to skip the bucket; `0s` turns it off |

Leave `allow_http` out and `AWS_ALLOW_HTTP` or `AZURE_ALLOW_HTTP` in the environment decides, as
it does for any `object_store` client. Set it, and the file wins: `false` refuses a plain-HTTP
endpoint even when the environment allows one.

With `async` and no `[disk]`, there is no local copy to upload from later, so stores go to the
bucket inline, as with `sync`, and the node logs a warning at startup.

`upload_queue` must be at least 1 with `write = "async"`. With `sync`, a failed upload still
answers the store with success, since the entry is stored on the node and Gradle would turn its
cache off over an error; the failure is logged and counted in
`mammoth_bucket_uploads_total{result="error"}`.

A bucket outage costs hits, not errors or stalls. A bucket read that fails or takes longer than
`timeout` is a miss, well inside Gradle's 30 s request timeout. After any bucket failure the node
skips the bucket for `cooldown`: reads miss at once, and uploads are dropped and counted in
`mammoth_bucket_uploads_dropped_total{reason="bucket-down"}`. The first read after the cooldown
tries the bucket again. See [Health checks](health.md#bucket-outages) for what to alert on.

Credentials come from the environment, never from the file: workload identity (EKS IRSA or Pod
Identity, GKE, AKS), instance roles and VM service accounts, or keys in the cloud's usual variables
(`AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` for S3-compatible stores). AWS profiles and SSO are
not read. Design doc [0003](../design/0003-architecture.md#bucket-access) lists what works, and
[Bucket access](deployment.md#bucket-access) shows the setup. Entries are never deleted from the
bucket; set a lifecycle rule, for example 30 days, to expire them.

### `[auth]`

Users, tokens and scopes. Without users, anonymous scopes or `metrics.require_auth`, the node is
open to everyone, as a fresh install should be. See [Authentication](authentication.md).

| Key                         | Type            | Default  | Example                            | Status    | Meaning                                   |
| --------------------------- | --------------- | -------- | ---------------------------------- | --------- | ----------------------------------------- |
| `anonymous`                 | list of scopes  | `[]`     | `["cache:read"]`                   | Available | Scopes for requests without credentials   |
| `users.<name>.scopes`       | list of scopes  | required | `["cache:read", "cache:write"]`    | Available | What the user may do                      |
| `users.<name>.token_hashes` | list of strings | required | `["sha256:9f86…0a08"]`             | Available | `sha256:` hashes from `mammoth token new` |
| `users.<name>.note`         | string          | none     | `"Continuous integration"`         | Available | Free text                                 |

The scopes are `cache:read`, `cache:write`, `metrics:read` and `admin:read`. `anonymous` is the
Build Cache Node's `anonymousLevel`. Each user needs at least one token hash; any of them
authenticates, so a token can be rotated by adding the new hash before removing the old one.

### `[metrics]`

| Key            | Type    | Default | Example | Status    | Meaning                                               |
| -------------- | ------- | ------- | ------- | --------- | ----------------------------------------------------- |
| `require_auth` | boolean | `false` | `true`  | Available | Require credentials with `metrics:read` on `/metrics` |

### `[log]`

| Key            | Type    | Default  | Example   | Status       | Meaning                                                                   |
| -------------- | ------- | -------- | --------- | ------------ | ------------------------------------------------------------------------- |
| `level`        | string  | `info`   | `"debug"` | Available    | `error`, `warn`, `info`, `debug` or `trace`                               |
| `format`       | string  | `json`   | `"text"`  | Available    | `json` or `text`                                                          |
| `access_log`   | boolean | `true`   | `false`   | Available    | Log each request, with the user name, never the token                     |
| `buffer_lines` | integer | `128000` | `500000`  | Available    | Lines buffered for stdout; when it is full, lines are dropped and counted |

`RUST_LOG`, when set, replaces `log.level`. The line format, and how to collect the logs, are in
[Logs](logs.md).

## Full example

Every key, with its default where it has one. The file is
[`mammoth.example.toml`](mammoth.example.toml) in the repository, and it is checked to parse and
to set every key.

```toml
{{#include mammoth.example.toml}}
```

## Secrets

Nothing in the file is secret. It holds token hashes, not tokens, and bucket credentials come from
the environment. It can live in version control and be readable by anyone who runs the node.
