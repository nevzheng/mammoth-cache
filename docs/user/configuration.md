# Configuration reference

mammoth-cache reads one TOML file at startup. If no path is given and `./mammoth.toml` doesn't
exist, the node runs on the defaults with memory only, so `docker run` works without a file, but
entries are lost on restart, and entries over `memory.max_entry_size` are treated as oversized.
[Not enforced yet](#not-enforced-yet) lists the keys that are validated but don't take effect yet.

## The config file

mammoth-cache reads one TOML file at startup and never writes to it. To change a setting, edit the
file and restart the node. On Kubernetes, mount the file from a Secret, since it holds token
hashes; a rollout restarts the pods.

The file is found at the path given by `--config`, else `MAMMOTH_CONFIG`, else `./mammoth.toml`.
That is the only flag or variable that affects settings: there are no per-key overrides. The one
exception is `RUST_LOG`, which replaces `log.level` when set, for debugging one module.

Every key except `version` and `disk.path` is optional. Unknown keys, wrong types, bad units and
contradictions are errors at startup. `mammoth config check` runs the same validation without
starting the server.

### Units

- Sizes take binary suffixes: `KiB`, `MiB`, `GiB`, `TiB`, for example `"50GiB"`.
- Durations take `s`, `m`, `h` or `d`, for example `"30d"`.

### Example

```toml
version = 1
listen = "0.0.0.0:5071"

[limits]
max_entry_size = "100MiB"

[memory]
size = "1GiB"
max_entry_size = "1MiB"

[disk]
path = "/var/lib/mammoth"
size = "50GiB"
free_space_buffer = "1GiB"
max_entry_age = "30d"

[bucket]
url = "s3://acme-build-cache/mammoth/"
region = "us-east-1"

[auth]
anonymous = []

[auth.users.ci]
note = "Continuous integration"
scopes = ["cache:read", "cache:write"]
token_hashes = ["sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"]

[log]
level = "info"
format = "json"
```

## Keys

### Top level

| Key       | Type    | Default        | Meaning                                                                        |
| --------- | ------- | -------------- | ------------------------------------------------------------------------------ |
| `version` | integer | required       | Schema version of the file; `1`                                                |
| `listen`  | string  | `0.0.0.0:5071` | Address and port; 5071 is the Build Cache Node's port. Use `[::]:5071` on IPv6 |

### `[protocols]`

Which clients the node serves. A protocol that is off has no routes: its requests get `404`, like
any unknown path. Health checks, metrics and the admin API are always served. At least one
protocol must be on.

| Key      | Type    | Default | Meaning                                                 |
| -------- | ------- | ------- | ------------------------------------------------------- |
| `gradle` | boolean | `true`  | Gradle's build cache at `/cache/`                       |
| `maven`  | boolean | `true`  | The Maven Build Cache Extension at `/maven/`            |
| `bazel`  | boolean | `false` | Bazel's HTTP cache at `/bazel/`; see [Bazel](bazel.md)  |

### `[server]`

How connections time out and how the node shuts down. Design doc
[0013](../design/0013-deployment.md) explains the choices.

| Key                 | Type     | Default | Meaning                                                                          |
| ------------------- | -------- | ------- | -------------------------------------------------------------------------------- |
| `shutdown_delay`    | duration | `5s`    | After `SIGTERM`, keep serving while load balancers see `/readyz` fail            |
| `shutdown_timeout`  | duration | `25s`   | From `SIGTERM`, the most time to finish requests and queued bucket uploads       |
| `idle_timeout`      | duration | `75s`   | How long a connection may wait for, and take to send, its next request's headers |
| `body_read_timeout` | duration | `60s`   | How long an upload may send nothing before it fails; per read, not in total      |

On `SIGTERM` or Ctrl-C the node fails `/readyz`, keeps serving for `shutdown_delay`, stops
accepting connections, finishes requests in flight, then finishes queued bucket uploads, all
within `shutdown_timeout`. A second signal exits at once. Keep `shutdown_timeout` below the
platform's grace period (30 s on Kubernetes), and `shutdown_delay` below `shutdown_timeout`.

Keep `idle_timeout` above the load balancer's idle timeout (60 s on AWS ALB and nginx), or the
load balancer reuses a connection the node has just closed and returns `502`. Google Cloud load
balancers keep connections 600 s, so raise it there.

### `[tls]`

Optional. With it, the node serves HTTPS on `listen` instead of plain HTTP (TLS 1.2 and 1.3,
HTTP/1.1). Without it, serve plain HTTP behind a proxy that terminates TLS; with authentication on,
the node then logs a warning at startup.

| Key    | Type | Default  | Meaning                                                                      |
| ------ | ---- | -------- | ---------------------------------------------------------------------------- |
| `cert` | path | required | PEM certificate chain: the server certificate first, then any intermediates  |
| `key`  | path | required | PEM private key, unencrypted: PKCS#8, PKCS#1 (RSA) or SEC1 (EC)              |

Startup, and `mammoth config check`, fail if either file can't be read or the key doesn't match the
certificate. The node re-reads both files every minute and picks up a renewed pair without a
restart; see [TLS on the node](deployment.md#tls-on-the-node).

### `[limits]`

| Key              | Type | Default  | Meaning                                                                                       |
| ---------------- | ---- | -------- | --------------------------------------------------------------------------------------------- |
| `max_entry_size` | size | `100MiB` | Largest entry stored. Gradle stores over it are accepted and dropped; Maven stores get `413`. |

### `[memory]` (L1)

| Key              | Type   | Default   | Meaning                                                                  |
| ---------------- | ------ | --------- | ------------------------------------------------------------------------ |
| `size`           | size   | `1GiB`    | Memory for entries                                                       |
| `max_entry_size` | size   | `1MiB`    | Larger entries skip memory, so one big entry can't flush many small ones |
| `eviction`       | string | `s3-fifo` | `s3-fifo`, `lru`, `w-tinylfu`, `sieve` or `fifo`                         |

`memory.max_entry_size` may not be larger than `limits.max_entry_size`. Design doc
[0010](../design/0010-memory-tier-and-eviction.md) explains the eviction policies.

### `[disk]` (L2)

| Key                 | Type     | Default  | Meaning                                                       |
| ------------------- | -------- | -------- | ------------------------------------------------------------- |
| `path`              | path     | required | Directory for entries; each node needs its own                |
| `size`              | size     | `10GiB`  | Space for entries                                             |
| `free_space_buffer` | size     | `1GiB`   | Free space to keep on the volume; the tier shrinks to keep it |
| `max_entry_age`     | duration | none     | Evict entries older than this                                 |

Entries are written to a temporary file and renamed into place, so a crash never leaves a partial
entry. Each file carries a CRC32C checksum, checked on every read; a damaged entry is treated as a
miss and removed. On startup the node indexes the files already on disk, so a restarted node
starts warm.

Every 5 seconds the node writes and removes a probe file and reads the volume's free space. If free
space drops below `free_space_buffer`, it evicts entries until the buffer is free again, and
`/readyz` fails while it cannot. Those results are what `/readyz` reports.

### `[bucket]` (L3)

Optional. With a bucket, every entry is uploaded to it, and several nodes can share it. The bucket
tier is built on the `object_store` crate.

| Key            | Type    | Default              | Meaning                                                                |
| -------------- | ------- | -------------------- | ---------------------------------------------------------------------- |
| `url`          | string  | required             | `s3://`, `gs://`, `az://` or `file://` URL, with an optional prefix    |
| `region`       | string  | from the environment | Region, for S3                                                         |
| `endpoint`     | string  | none                 | Endpoint for S3-compatible stores such as MinIO or R2                  |
| `allow_http`   | boolean | `false`              | Allow an `http://` endpoint, such as a local MinIO, SeaweedFS, Azurite |
| `write`        | string  | `async`              | `async` answers a store once it's on disk; `sync` waits for the upload |
| `upload_queue` | integer | `1000`               | Uploads waiting at once, for `async`; more are dropped and counted     |

Credentials come from the environment, never from the file: workload identity (EKS IRSA or Pod
Identity, GKE, AKS), instance roles and VM service accounts, or keys in the cloud's usual variables
(`AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` for S3-compatible stores). AWS profiles and SSO are
not read. Design doc [0003](../design/0003-architecture.md#bucket-access) lists what works. Entries
are never deleted from the bucket; set a lifecycle rule, for example 30 days, to expire them.

### `[auth]`

See [Authentication](authentication.md).

| Key                         | Type             | Default  | Meaning                                    |
| --------------------------- | ---------------- | -------- | ------------------------------------------ |
| `anonymous`                 | list of scopes   | `[]`     | Scopes for requests without credentials    |
| `users.<name>.scopes`       | list of scopes   | required | What the user may do                       |
| `users.<name>.token_hashes` | list of strings  | required | `sha256:` hashes from `mammoth token new`  |
| `users.<name>.note`         | string           | none     | Free text                                  |

### `[metrics]`

| Key            | Type    | Default | Meaning                                                      |
| -------------- | ------- | ------- | ------------------------------------------------------------ |
| `require_auth` | boolean | `false` | Require a Bearer token with `metrics:read` on `/metrics`     |

### `[log]`

| Key          | Type    | Default | Meaning                                               |
| ------------ | ------- | ------- | ----------------------------------------------------- |
| `level`      | string  | `info`  | `error`, `warn`, `info`, `debug` or `trace`           |
| `format`     | string  | `json`  | `json` or `text`                                      |
| `access_log` | boolean | `true`  | Log each request, with the user name, never the token |

## Secrets

Nothing in the file is secret. It holds token hashes, not tokens, and bucket credentials come from
the environment. It can live in version control and be readable by anyone who runs the node.

## Not enforced yet

mammoth-cache is pre-alpha. These keys are read and validated but don't take effect yet:

- `log.access_log`.
