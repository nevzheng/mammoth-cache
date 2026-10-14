# Configuration reference

> [!NOTE]
> The config file is planned and not read yet. Today the only settings are two environment
> variables, listed under [Settings available today](#settings-available-today). The rest of this
> page is the reference for `mammoth.toml` as designed in design doc
> [0005](../design/0005-configuration.md), so you can plan a deployment.

## Settings available today

| Variable       | Default        | Meaning                                                                                                                      |
| -------------- | -------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| `MAMMOTH_ADDR` | `0.0.0.0:5071` | Address and port to listen on                                                                                                |
| `RUST_LOG`     | `info`         | Log filter, in [`tracing` syntax](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html) |

The entry size limit is fixed at 100 MiB. The memory tier is a placeholder with no size limit or
eviction: `/v1/serverStatus` reports a 1 GiB capacity, but it isn't enforced yet. Both variables go
away when the config file lands.

## The config file

mammoth-cache reads one TOML file at startup and never writes to it. To change a setting, edit the
file and restart the node. On Kubernetes, mount the file from a ConfigMap; a rollout restarts the
pods.

The file is found at the path given by `--config`, else `MAMMOTH_CONFIG`, else `./mammoth.toml`.
That is the only flag or variable that affects settings: there are no per-key overrides.

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

| Key       | Type    | Default        | Meaning                                                  |
| --------- | ------- | -------------- | -------------------------------------------------------- |
| `version` | integer | required       | Schema version of the file; `1`                          |
| `listen`  | string  | `0.0.0.0:5071` | Address and port. 5071 is the Build Cache Node's port.   |

### `[tls]`

Optional. Omit it to serve plain HTTP, for example behind a proxy that terminates TLS. With
authentication on and no TLS, the node logs a warning at startup.

| Key    | Type   | Default  | Meaning                    |
| ------ | ------ | -------- | -------------------------- |
| `cert` | path   | required | PEM certificate chain      |
| `key`  | path   | required | PEM private key            |

### `[limits]`

| Key              | Type | Default  | Meaning                                                                                       |
| ---------------- | ---- | -------- | --------------------------------------------------------------------------------------------- |
| `max_entry_size` | size | `100MiB` | Largest entry stored. Gradle stores over it are accepted and dropped; Maven stores get `413`. |

### `[memory]` (L1)

| Key              | Type   | Default   | Meaning                                                                  |
| ---------------- | ------ | --------- | ------------------------------------------------------------------------ |
| `size`           | size   | `1GiB`    | Memory for entries                                                       |
| `max_entry_size` | size   | `1MiB`    | Larger entries skip memory, so one big entry can't flush many small ones |
| `eviction`       | string | `s3-fifo` | Proposed: `s3-fifo`, `lru`, `w-tinylfu`, `sieve` or `fifo`               |

`memory.max_entry_size` may not be larger than `limits.max_entry_size`. The `eviction` key is
proposed in [PR #13](https://github.com/nevzheng/mammoth-cache/pull/13), under review.

### `[disk]` (L2)

| Key                 | Type     | Default  | Meaning                                                      |
| ------------------- | -------- | -------- | ------------------------------------------------------------ |
| `path`              | path     | required | Directory for entries; each node needs its own               |
| `size`              | size     | `10GiB`  | Space for entries, or `"max-available"` for the whole volume |
| `free_space_buffer` | size     | `1GiB`   | Free space to leave on the volume                            |
| `max_entry_age`     | duration | none     | Evict entries older than this                                |

The disk tier is built (`mammoth_cache::store::disk::DiskStore`), but the `mammoth` binary turns it
on only once the config file lands; a [custom build](custom-builds.md) can use it today.

Entries are written to a temporary file and renamed into place, so a crash never leaves a partial
entry. Each file carries a CRC32C checksum, checked on every read; a damaged entry is treated as a
miss and removed. On startup the node indexes the files already on disk, so a restarted node
starts warm.

### `[bucket]` (L3)

Optional. With a bucket, every entry is uploaded to it, and several nodes can share it.

| Key            | Type    | Default              | Meaning                                                                |
| -------------- | ------- | -------------------- | ---------------------------------------------------------------------- |
| `url`          | string  | required             | `s3://`, `gs://`, `az://` or `file://` URL, with an optional prefix    |
| `region`       | string  | from the environment | Region, for S3                                                         |
| `endpoint`     | string  | none                 | Endpoint for S3-compatible stores such as MinIO or R2                  |
| `write`        | string  | `async`              | `async` answers a store once it's on disk; `sync` waits for the upload |
| `upload_queue` | integer | `1000`               | Uploads waiting at once, for `async`; more are dropped and counted     |

Credentials come from the cloud's default credential chain (environment, instance or workload
identity), never from the file. Entries are never deleted from the bucket; set a lifecycle rule,
for example 30 days, to expire them.

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
