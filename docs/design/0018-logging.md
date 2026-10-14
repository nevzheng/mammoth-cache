# 0018: Logging

## Status

Accepted

## Summary

The node writes two kinds of logs to one place. **Operational logs** are the node reporting on
itself: startup, configuration, a tier that fails or recovers, shutdown. **Access logs** are one
record per HTTP request or gRPC call. Both go through `tracing` and `tracing-subscriber`, already in
use, as one JSON object per line on stdout. Access lines are a separate topic in that stream
(target `mammoth::access`), not a separate stream.

Field names follow the OpenTelemetry semantic conventions (`http.request.method`, `url.path`,
`client.address`), with the node's own fields under `mammoth.`. The envelope (`timestamp`,
`severity`, `message`) uses the names that Google Cloud Logging, Datadog, Grafana Loki and the
OpenTelemetry log data model read without any mapping. The schema is documented in the user guide
and enforced by a test; renaming a field is a breaking change.

The node does not rotate, ship or buffer logs to disk. The container runtime, journald or a log
agent does that, and the user guide documents each of them. Export over OTLP is a possible later
build feature, not part of 0.1.0.

## Context and scope

Today the node logs with `tracing` (64 call sites) through `tracing_subscriber::fmt`, as JSON
(default) or text, at `log.level`. Field names are ad hoc (`key`, `tier`, `error`, `path`,
`addr`), the JSON shape is `tracing-subscriber`'s default (fields nested under `fields`, level as
`WARN`), and some warnings fire once per request, so a bucket outage can write one line per cache
miss. `log.access_log` is accepted and documented as "not enforced" (issue #86).

Operators coming from the Gradle Build Cache Node get `«data-dir»/logs/node.log`, rotated, with no
documented format or level setting. Its docs say it logs `X-Amzn-Trace-Id` and honours
`X-Forwarded-For`. bazel-remote has `access_log_level = none | all`, one text line per request.
No cache protocol says what a server should log, because clients only see status codes. That
leaves the format to us, and the bar is set by the log platforms operators already run.

This doc covers the log format, levels, what is and is not logged, the access log, how logs reach
log platforms, and the order of the pull requests. Metrics stay in [0004](0004-metrics.md); logs
never replace a metric.

## Goals and non-goals

### Goals

- One stable, documented line format that every major log platform parses with no
  per-platform configuration: severity, timestamp and message land in the right place.
- Field names an operator can guess, taken from OpenTelemetry where a convention exists.
- Access log answers "why did this build miss?" and "who wrote this key, from where?".
- An outage writes a handful of lines, not one per request.
- Secrets never reach a log line.
- Bounded cost: one formatted line per request, measured.

### Non-goals

- Log files, rotation or retention inside the node.
- Vendor-specific sinks (a Datadog or CloudWatch client in the binary).
- Distributed tracing. Trace ids from the load balancer are logged for correlation, but the node
  creates no spans for export.
- A formal JSON Schema file, and log-based alerting. Alerts come from metrics
  ([0015](0015-metrics-rollout-and-testing.md)).
- Build telemetry or client-side analytics.

## Design objectives and constraints

In priority order: no secrets in logs; parsed correctly by default on the common platforms;
negligible cost per request; stable for anyone who builds dashboards on it; simple to configure.

## Design

### Library

`tracing` for call sites and `tracing-subscriber` for output, as today. Both are the standard for
tokio and axum services, carry structured fields and spans, and have first-party OpenTelemetry
bridges (`tracing-opentelemetry`, `opentelemetry-appender-tracing`) if export is added later. The
older `log` facade is still bridged by `tracing-subscriber`, so dependencies that use it keep
working. `slog` and lower-level tracers such as `fastrace` were not considered further: neither
adds anything the node needs.

### Where logs go

- Everything goes to **stdout**, one line per event, in `log.format`: `json` (default) or `text`.
- Docker, Kubernetes, ECS, Cloud Run and systemd all capture stdout. Fluent Bit, Vector, Grafana
  Alloy, the Datadog agent, the CloudWatch agent and the Google Ops Agent all read it. A log file
  would need a volume, rotation and an agent configured to tail it.
- Rotation belongs to whatever runs the process: the kubelet rotates container logs (by default
  at 10 MiB, keeping 5 files), Docker's `json-file` and `local` drivers take `max-size` and
  `max-file`, and journald rotates by size. The user guide gives the settings.
- Rotated logs that share a volume with `disk.path` (a bare-metal host, or a future log file) are
  protected by `disk.free_space_buffer` (default 1 GiB, the Build Cache Node's
  `freeSpaceBufferSize`): the disk tier evicts whenever free space on the volume drops below it,
  and with `disk.size = "max-available"` the tier takes the volume minus the buffer. The Logs page
  says to raise the buffer above the logs' rotation limit when they share the volume.
- No log file by default (Nevin, 2026-10-14). If a `log.file` option is ever added, it is off
  unless set, and the node writes nothing to disk for logging otherwise.
- Writes are synchronous through a line-buffered stdout. If the load test shows the access log
  costs more than 5% of p99 latency or throughput, switch to `tracing-appender`'s non-blocking
  writer and count dropped lines in a metric (open question 3).

### The JSON line

A small formatter of our own (a `tracing-subscriber` `FormatEvent`, about a hundred lines) writes
a flat object:

```json
{"timestamp":"2026-10-14T15:01:02.345678Z","severity":"WARNING","target":"mammoth::cache",
 "message":"bucket failed; serving from the local tiers until it answers again",
 "mammoth.cache.key":"gradle/4f1c…","exception.message":"connection refused",
 "mammoth.cooldown_s":30}
```

| Field       | Content                                           | Why this name                                                                        |
| ----------- | ------------------------------------------------- | ------------------------------------------------------------------------------------ |
| `timestamp` | RFC 3339, UTC, microseconds                       | Read as the event time by Cloud Logging, Datadog, Loki and Elastic                   |
| `severity`  | `DEBUG`, `INFO`, `WARNING`, `ERROR`               | Cloud Logging's severity field and values; one of Datadog's default status fields    |
| `target`    | The Rust module, or `mammoth::access`             | The topic: route or filter access lines by it                                        |
| `message`   | Human-readable text                               | The message field on every platform                                                  |
| `trace_id`  | 32 hex digits, when known                         | OpenTelemetry log data model; joins the line to the load balancer's log              |
| others      | Flat, dotted attribute names (below)              | OpenTelemetry attribute names, as an OTLP exporter would write them                  |

`TRACE` events are written with `severity` `DEBUG`, since Cloud Logging has no trace level. The
formatter writes no span lists and no thread ids. Attribute values are JSON strings, numbers or
booleans; anything else is written with `Display`.

Why our own formatter rather than `tracing-subscriber`'s `json()`: the built-in one nests fields
under `fields` (unless flattened), names the level `level` with value `WARN`, and adds span
lists. Each of those needs a mapping rule on some platform. Owning the formatter also makes the
schema test possible: the formatter is the only place a top-level key is written.

The `text` format stays `tracing-subscriber`'s default, for people reading a terminal. It is not
part of the schema.

### Attribute names

Where OpenTelemetry has a convention, use it. Where it has none, use `mammoth.` plus a name in the
same style. The vocabulary:

| Meaning                     | Attribute                       | Source                           |
| --------------------------- | ------------------------------- | -------------------------------- |
| An error's text             | `exception.message`             | OTel exceptions                  |
| An error's kind, when known | `error.type`                    | OTel errors                      |
| Cache key                   | `mammoth.cache.key`             | ours                             |
| Tier                        | `mammoth.cache.tier`            | ours: `memory`, `disk`, `bucket` |
| File or directory           | `file.path`                     | OTel file                        |
| Listening address           | `server.address`, `server.port` | OTel server                      |
| User name                   | `user.name`                     | OTel user                        |
| Durations                   | `mammoth.<what>_ms` or `_s`     | ours; unit in the name           |

Existing call sites move to this vocabulary in one pull request. `tracing` accepts dotted field
names, so a call site reads `tracing::warn!(mammoth.cache.key = %key, exception.message = %e, ...)`.

### Levels

| Level   | Meaning                                                       | Examples                                                            |
| ------- | ------------------------------------------------------------- | ------------------------------------------------------------------- |
| `ERROR` | The node cannot do part of its job and an operator must act   | Disk not writable, bucket upload worker died                        |
| `WARN`  | Degraded but serving, or a setting that is probably a mistake | Bucket down (once), damaged entry, certificate expiring, no auth    |
| `INFO`  | Lifecycle and state changes; every access log line            | Started, listening, config loaded, bucket answering again, draining |
| `DEBUG` | Per-request detail and expected client misbehaviour           | Failed TLS handshakes, each failed bucket call during an outage     |
| `TRACE` | Development only                                              |                                                                     |

`log.level` sets operational verbosity (default `info`). The access log has its own switch
(`log.access_log`, default `true`) and is written whatever `log.level` says, so `level = "warn"`
quietens the node without losing the request log. `RUST_LOG`, when set, replaces both: it is the
debugging escape hatch (`RUST_LOG=info,mammoth::access=off`).

### Log state changes, not occurrences

A failure that can repeat per request logs a `WARN` when it starts and an `INFO` when it ends;
each occurrence in between is `DEBUG` and counted by a metric. The bucket's health tracking and
the accept loop already do this. The audit in pull request 2 covers the rest: tier read and head
failures, secondary-tier write failures, storage errors rendered by the HTTP and gRPC adapters,
and dropped uploads.

Where there is no natural state, a small per-call-site throttle applies: the first occurrence is
logged, then at most one line per 10 seconds carrying `mammoth.suppressed` (how many were
skipped). It is an atomic timestamp and counter per call site, no lock.

### Never logged

- Tokens, the `Authorization` header, any request header other than `User-Agent` and the trace
  headers, cookies, query strings, bucket credentials or signed URLs.
- A user name that failed to authenticate: it may be a token pasted into the wrong field. Those
  requests log `user.name` `anonymous` and `mammoth.cache.result` `refused`.
- Entry contents.

Client-supplied strings (`User-Agent`, paths) are escaped by the JSON writer and quoted in the text
format, so they cannot forge a line. A test runs requests with random tokens through every
protocol and checks that no token appears in any line.

### Access log

One `INFO` line on target `mammoth::access` per finished HTTP request or gRPC call, written when
the response body ends or the client goes away, so sizes and duration cover the whole transfer.
Health and scrape endpoints (`/ping`, `/livez`, `/readyz`, `/metrics`) are not logged.

HTTP:

| Attribute                   | Example           | Notes                                                                   |
| --------------------------- | ----------------- | ----------------------------------------------------------------------- |
| `http.request.method`       | `GET`             |                                                                         |
| `url.path`                  | `/cache/4f1c…`    | No query string. Keys are hashes and Maven coordinates, not secrets     |
| `http.route`                | `/cache/{key}`    | The route template, for grouping                                        |
| `http.response.status_code` | `200`             |                                                                         |
| `http.request.body.size`    | `0`               | Bytes read                                                              |
| `http.response.body.size`   | `52113`           | Bytes sent                                                              |
| `client.address`            | `10.2.3.4`        | The client, after trusted proxies (below)                               |
| `network.peer.address`      | `10.0.0.7`        | The TCP peer, when different from `client.address`                      |
| `user_agent.original`       | `Gradle/8.10 (…)` |                                                                         |
| `user.name`                 | `ci`              | Or `anonymous`                                                          |
| `mammoth.protocol`          | `gradle`          | `gradle`, `maven`, `bazel`, `admin`, `other`                            |
| `mammoth.cache.result`      | `hit`             | Cache routes: `hit`, `miss`, `stored`, `exists`, `too_large`, `refused` |
| `mammoth.cache.tier`        | `disk`            | On a hit: the tier that served it                                       |
| `mammoth.duration_ms`       | `3.214`           | Request start to last byte or disconnect, to the microsecond            |
| `mammoth.aborted`           | `true`            | Only when the client went away mid-response                             |
| `trace_id`                  | `4bf92f35…`       | When the request carries one (below)                                    |

gRPC uses `rpc.system` (`grpc`), `rpc.service`, `rpc.method` and `rpc.grpc.status_code` from the
OpenTelemetry RPC conventions, plus the sizes, client, user, duration, `trace_id` and, for
`FindMissingBlobs`, `BatchReadBlobs` and `BatchUpdateBlobs`, `mammoth.digests` (how many).

`message` is `request` for HTTP and `call` for gRPC.

The HTTP body sizes are not yet marked stable in OpenTelemetry. They are used as published; if
the convention changes before it stabilises, the docs record the rename.

**Client address.** By default `client.address` is the TCP peer, with IPv4-mapped IPv6 shown as
IPv4. `server.trusted_proxies` (IP addresses or CIDRs, empty by default) changes that: when the
peer is in the list, `X-Forwarded-For` is read from the right, skipping trusted entries, and the
first untrusted address is the client. If every entry is trusted, the left-most is used; a
malformed entry stops the walk at the last good one. With the list empty the header is ignored, so
a client cannot choose its address in the log. `Forwarded` (RFC 7239) and `X-Real-IP` are not
read. Malformed CIDRs and CIDRs with host bits set (`10.0.0.1/8`) fail config validation.

**Trace id.** Taken from the first that is present: W3C `traceparent` (sent by Google Cloud load
balancers and OpenTelemetry-instrumented proxies), `X-Cloud-Trace-Context`, or AWS
`X-Amzn-Trace-Id` (its `Root=1-xxxxxxxx-yyyy…` becomes the 32-digit W3C form, the same conversion
X-Ray uses). Nothing is generated when none is present. Pull request 4 attaches it to operational
lines written while serving the request.

**Volume.** At 5,000 requests a second a line of about 400 bytes is 2 MB/s, roughly 170 GB a day.
The user guide says so next to `log.access_log`, with the kubelet and Docker settings that keep
rotation from losing lines, and the way to turn the access log off.

### Reaching log platforms

No platform code in the binary. The user guide gets a "Logs" page with the schema and short
recipes:

| Platform                      | What the operator does                                                                    |
| ----------------------------- | ----------------------------------------------------------------------------------------- |
| Google Cloud (GKE, Cloud Run) | Nothing: the built-in agent reads `severity`, `message` and `timestamp`                   |
| Datadog                       | Agent with container log collection; no pipeline needed for severity                      |
| Grafana Loki / Alloy          | `json` stage; label by `severity` and `target`                                            |
| AWS (ECS, EKS, EC2)           | `awslogs` driver or Fluent Bit to CloudWatch; Logs Insights reads the JSON fields         |
| Elastic                       | Filebeat or Elastic Agent with JSON decoding                                              |
| OpenTelemetry Collector       | `filelog` receiver with the JSON parser; `severity` and `timestamp` map to the data model |
| systemd                       | journald captures stdout; `journalctl -u mammoth -o cat`                                  |

Each recipe is checked against the platform's documentation when the page is written, not taken
from this table.

Later, behind a cargo feature and outside the default build: `otlp`, exporting the same events
over OTLP through `opentelemetry-appender-tracing`, configured by the standard `OTEL_EXPORTER_*`
environment variables. Only if someone asks; a collector reading stdout does the same job today.

## Cross-cutting concerns

- **Performance.** One formatted line per request on the serving task. Measured with the Locust
  steady run (0011), access log on and off; the numbers go in the access log pull request.
- **Security.** Covered under "Never logged". `trusted_proxies` stops clients spoofing their
  address.
- **Compatibility.** The JSON shape changes once, in pull request 1, before 0.1.0. After the
  release, the documented fields are a contract: adding a field is fine, renaming or removing one
  is a breaking change noted in the changelog.
- **Testing.** A schema test captures the formatter's output in process and checks every
  top-level key and every access log attribute against the tables in the user guide, the same way
  `config_doc.rs` checks configuration keys. The token property test above. An end-to-end check
  in the Python harness reads the container log after a Gradle build and finds the build's keys
  with `user.name` `ci`, and, through the nginx target with `trusted_proxies` set, finds the test
  runner's address in `client.address`.

## Rollout

1. **Formatter and schema.** The JSON formatter, the `severity` mapping, the user guide "Logs"
   page with the platform recipes, and the schema test. No new configuration.
2. **Operational logs.** Move every call site to the attribute vocabulary, apply the level table,
   and make repeating failures log state changes or throttle.
3. **Access log (#86).** The `mammoth::access` lines for HTTP and gRPC, `server.trusted_proxies`,
   trace ids, the load-test numbers, and the configuration reference marking `log.access_log`
   available.
4. **Correlation.** A request span carrying `trace_id`, so operational lines written while serving
   a request carry it too.

## Alternatives considered

- **A separate access log stream or file.** Platforms already route by a field, and a second
  destination means a path, rotation and a second agent input. A `log.access_log_path` (sized
  against `disk.free_space_buffer`, as above) can be added later for bare-metal installs if asked.
- **Our own short field names** (`method`, `path`, `client`). Shorter lines, but every platform
  then needs a mapping, and an OTLP export later would rename everything.
- **Nested JSON objects** (`{"http":{"request":{"method":"GET"}}}`) instead of dotted keys.
  Elastic prefers nesting; OTLP, Datadog and Cloud Logging handle dotted keys, and dotted keys are
  what an OpenTelemetry exporter writes. Flat keys also keep the formatter simple.
- **Google Cloud's `httpRequest` object** for access lines. Cloud Logging renders it nicely, but it
  is one vendor's schema, and its `latency` is a string (`"0.003s"`). A `log.format = "gcp"` that
  adds it is possible later.
- **Writing logs only over OTLP.** Every deployment would need a collector before it could see a
  single log line.
- **The `log` crate with `env_logger`.** Unstructured, and already superseded in this codebase.

## Open questions

1. Should the access log be thinned rather than only on or off, for example logging hits only at
   `DEBUG`, or sampling them? Recommendation: no for 0.1.0, revisit with real traffic.
2. Include `service.name` and `service.version` on every line, as OpenTelemetry resource
   attributes? Recommendation: no. Log agents add them from the deployment, and the startup line
   carries the version.
3. Blocking or non-blocking writer? Recommendation: blocking, and switch only if the load test
   shows a measurable cost.
