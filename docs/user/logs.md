# Logs

mammoth-cache writes its logs to stdout, one JSON object per line. It writes no log files and does
not rotate anything: the container runtime, journald or a log agent collects stdout and keeps it.
Design doc [0018](../design/0018-logging.md) explains the choices.

## The line format

```json
{"timestamp":"2026-10-14T15:20:31.004218Z","severity":"WARNING","target":"mammoth_cache::cache","message":"tier read failed; skipping","key":"gradle/4f1c9a0e6b2d7c3a","tier":"disk","error":"Input/output error (os error 5)"}
```

Every line starts with the same four fields, followed by the event's own:

| Field       | Content                                                                        |
| ----------- | ------------------------------------------------------------------------------ |
| `timestamp` | When it happened: RFC 3339, UTC, microseconds                                  |
| `severity`  | `DEBUG`, `INFO`, `WARNING` or `ERROR` (`trace` events are written as `DEBUG`)  |
| `target`    | Where it came from, a Rust module such as `mammoth_cache::cache`               |
| `message`   | What happened, in words                                                        |

Numbers and booleans are JSON numbers and booleans. Anything a client sent, such as a path or a
`User-Agent`, is escaped, so it cannot break a line or forge one. `log.format = "text"` writes
human-readable lines instead, for a terminal; that format is not stable.

The four envelope fields and their meaning are stable. The other field names are stable for the
same message, but messages can be reworded between releases.

## Levels

| Level     | Means                                                                                    |
| --------- | ---------------------------------------------------------------------------------------- |
| `ERROR`   | Part of the node isn't working and needs an operator, such as a disk that isn't writable |
| `WARNING` | Degraded but serving, or a setting that is probably a mistake                            |
| `INFO`    | Starting, listening, configuration, recovering, shutting down                            |
| `DEBUG`   | Per-request detail, and each repeat of a failure already reported                        |

`log.level` sets the most verbose level written (default `info`). `RUST_LOG`, when set, replaces
it, for example `RUST_LOG=info,mammoth_cache::cache=debug`.

A failure that can happen on every request, such as a failing disk or an unavailable bucket, is
logged at most once every 10 seconds per message. The repeats in between are `DEBUG`, and the next
line carries `suppressed`, the number held back. Metrics count every occurrence.

## Access log

Each HTTP request and gRPC call gets one `INFO` line with `target` `mammoth::access`, written when
the response ends or the client goes away, so the sizes and the duration cover the whole transfer.
`/ping`, `/livez`, `/readyz` and `/metrics` are not logged. Filter or route on `target` to keep the
access lines apart from the rest.

```json
{"timestamp":"2026-10-14T15:01:02.345678Z","severity":"INFO","target":"mammoth::access","message":"request","http.request.method":"GET","url.path":"/cache/4f1c9a0e6b2d7c3a","http.route":"/cache/{key}","http.response.status_code":200,"http.request.body.size":0,"http.response.body.size":52113,"client.address":"203.0.113.9","network.peer.address":"10.0.0.7","user_agent.original":"Gradle/8.10 (Linux;6.1;amd64)","user.name":"ci","mammoth.protocol":"gradle","mammoth.cache.key":"gradle/4f1c9a0e6b2d7c3a","mammoth.cache.result":"hit","mammoth.cache.tier":"memory","mammoth.duration_ms":3.214}
```

| Field                       | Content                                                                               |
| --------------------------- | ------------------------------------------------------------------------------------- |
| `http.request.method`       | The HTTP method                                                                       |
| `url.path`                  | The path, without the query string                                                    |
| `http.route`                | The route it matched, such as `/cache/{key}`; absent for an unknown path              |
| `http.response.status_code` | The status; absent when the client went away before the answer                        |
| `http.request.body.size`    | Bytes read from the request body                                                      |
| `http.response.body.size`   | Bytes of the response body sent                                                       |
| `client.address`            | The client: the connection's peer, or the address trusted proxies report (below)      |
| `network.peer.address`      | The connection's peer, when it is not the client                                      |
| `user_agent.original`       | The `User-Agent`                                                                      |
| `user.name`                 | The authenticated user, or `anonymous`                                                |
| `mammoth.protocol`          | `gradle`, `maven`, `bazel`, `admin`, `reapi` or `other`                               |
| `mammoth.cache.key`         | The entry, as operational lines name it                                               |
| `mammoth.cache.result`      | `hit`, `miss`, `stored`, `exists`, `too_large` (refused or dropped), or `refused`     |
| `mammoth.cache.tier`        | On a hit, the tier that served it, when known                                         |
| `mammoth.duration_ms`       | From the request's arrival to the response's last byte, or the client leaving         |
| `mammoth.aborted`           | `true` when the response did not finish: the client went away, or the response failed |

gRPC calls have `message` `call`, and `rpc.system` (`grpc`), `rpc.service`, `rpc.method` and
`rpc.grpc.status_code` (the number: `0` is `OK`, `5` is `NOT_FOUND`) in place of the HTTP request
fields. Their sizes count the gRPC messages as sent, and `FindMissingBlobs`, `BatchReadBlobs` and
`BatchUpdateBlobs` have `mammoth.digests`, the number of digests in the call. The key, result and
tier appear when a request touched exactly one entry. `user.name` is set only from a token the node
accepted: credentials it refuses are logged as `anonymous`, because what they carry as a user name
may be a token.

`log.access_log = false` turns the access log off. It is written whatever `log.level` says; with
`RUST_LOG` set, include it, as in `RUST_LOG=warn,mammoth::access=info`.

### Client addresses

By default `client.address` is the address the connection comes from, which behind a load
balancer is the load balancer's. List the load balancers in
[`server.trusted_proxies`](configuration.md#server), as addresses or CIDRs:

```toml
[server]
trusted_proxies = ["10.0.0.0/8"]
```

For a connection from one of them, `X-Forwarded-For` is read from the right, past every trusted
proxy, and the first address that is not one is the client. A client can write anything into the
header, but only the entries the trusted proxies added are used. With the list empty the header is
ignored.

## What is never logged

Tokens, the `Authorization` header, cookies, query strings, entry contents and bucket credentials.
A test sends requests with random tokens to every route, in every credentials format, and fails if
any part of a token reaches a log line.

## When stdout can't keep up

Lines are handed to a background thread that writes them, so a slow log pipe never slows down a
request. The thread holds up to `log.buffer_lines` lines (default 128,000). If the buffer fills,
new lines are dropped and counted in `mammoth_log_lines_dropped_total`. Logs are best effort;
metrics are the record to alert on.

## Collecting logs

Every major log platform collects container stdout with its own agent. What differs is whether it
reads the four envelope fields without setup:

| Platform                                     | `severity`     | `timestamp`                         | Setup                                |
| -------------------------------------------- | -------------- | ----------------------------------- | ------------------------------------ |
| Google Cloud Logging                         | read           | not read; the time received is used | none on GKE and Cloud Run            |
| Datadog                                      | read           | read                                | the agent's container log collection |
| Grafana Loki                                 | read           | needs a `stage.timestamp`           | Alloy, as below                      |
| Elastic, OpenTelemetry Collector, CloudWatch | needs a parser | needs a parser                      | see their container log guides       |

### Rotation

Container defaults keep very little. The kubelet keeps 5 files of 10 MiB per container, and Docker's
default `json-file` driver never rotates. A busy node can write megabytes of logs a second, so:

- run a log agent that ships logs off the node;
- on Kubernetes, raise the kubelet's `containerLogMaxSize` (for example `100Mi`);
- on Docker, use the `local` driver or set `max-size` and `max-file`:

```json
{"log-driver": "local", "log-opts": {"max-size": "100m", "max-file": "5"}}
```

If the node's logs and `disk.path` share a volume, keep the logs' total size within
`disk.free_space_buffer`: the disk tier evicts when free space drops below it, but nothing stops
logs filling it.

### Google Cloud

Nothing to set up on GKE or Cloud Run: severity and message are read from each line, and the other
fields are in `jsonPayload`. Quote dotted field names in queries:

```text
jsonPayload.target="mammoth_cache::cache" AND severity>=WARNING
```

### Datadog

Turn on container log collection in the agent (`DD_LOGS_ENABLED=true`,
`DD_LOGS_CONFIG_CONTAINER_COLLECT_ALL=true`) and tag the pod:

```yaml
ad.datadoghq.com/mammoth.logs: '[{"source":"mammoth","service":"mammoth-cache"}]'
```

Datadog reads `timestamp`, `severity` and `message` on its own.

### Grafana Loki

Loki detects the level from `severity`. To use the line's own time, add a timestamp stage in Alloy:

```alloy
loki.process "mammoth" {
  stage.json {
    expressions = { ts = "timestamp" }
  }
  stage.timestamp {
    source = "ts"
    format = "RFC3339Nano"
  }
  forward_to = [loki.write.default.receiver]
}
```

Then query with `| json`, which turns a dot in a field name into `_`.
