# 0015: Metrics rollout and testing

## Status

Draft

## Summary

Enough of the node exists to record the metrics in [0004](0004-metrics.md): the write path runs
through all three tiers, lookups fall through and promote, auth rejects requests, and `/metrics`
already renders what little is recorded. This doc checks 0004 against what `main` emits today,
orders the missing instrumentation into small pull requests, and plans the two kinds of tests that
keep the catalog honest: hermetic Rust tests that scrape the in-process `/metrics` after a workload,
and end-to-end checks in the Python harness from
[0011](https://github.com/nevzheng/mammoth-cache/pull/27) that read the same numbers through a real
Prometheus.

Telemetry here means the node's own Prometheus metrics. Build-scan style client telemetry is out of
scope. `User-Agent` and `X-Gradle-Version` are label sources and nothing more.

## Context and scope

0004 is the schema: a metric's name, type and labels are a contract. 0004 was written before the
tiers existed, so most of its catalog is unimplemented and two metrics shipped that it never
listed. Nothing tests that the catalog and the code agree, in either direction.

The code recording metrics belongs to the framework thread (`crates/mammoth-cache/src`). The
end-to-end suite belongs to the E2E thread (`tests/`, pull request #27). This doc is the plan both
work from; the hermetic metrics test lives with it.

## Goals and non-goals

### Goals

- Every metric in 0004 is recorded, with the labels 0004 gives it, before the first release.
- A test fails when the code emits a `mammoth_` metric the catalog does not list, when a catalogued
  metric that should be live is missing, or when a label value falls outside its bounded set.
- The same checks run against the real image, through a real Prometheus, in the end-to-end suite.
- Instrumentation stays off the hot path: one atomic increment per event, no locks, no scans.

### Non-goals

- Aggregation, rates or history in the node ([0004](0004-metrics.md)).
- Tracing or OpenTelemetry.
- Dashboards. Grafana in the Compose stack is for poking at load runs, not a deliverable.

## Design objectives and constraints

1. Catalog first: the code follows 0004. Where the code found a better shape, 0004 changes in the
   same pull request, and the change is called out as breaking or additive.
2. Instrument the ports, not the adapters: tier metrics are recorded once in the core around the
   `BlobStore` port, so memory, disk and bucket cannot drift apart.
3. The `metrics` facade stays. It is on `main`, every store already records through it, and
   `metrics-exporter-prometheus` renders `/metrics`. 0004's open question closes in its favour.

## Design

### Gap check: 0004 against `main`

Checked at `c9daff9` (2026-10-14). "Source" is where the number already exists in the code.

| Metric                                  | On `main`                    | Source today                                          |
| --------------------------------------- | ---------------------------- | ----------------------------------------------------- |
| `mammoth_http_requests_total`           | yes                          | `http::metrics::record`                               |
| `mammoth_gradle_requests_total`         | yes                          | `http::metrics::record`                               |
| `mammoth_tier_promotions_total`         | yes                          | `cache::promote`                                      |
| `mammoth_tier_evictions_total`          | memory only, `reason="size"` | `store::memory::eviction_counter`                     |
| `mammoth_cache_integrity_errors_total`  | disk only                    | `store::disk` verify-before-serve                     |
| `mammoth_bucket_upload_queue_entries`   | yes                          | `cache::enqueue_upload`, `upload_worker`              |
| `mammoth_bucket_uploads_total`          | yes, **not in 0004**         | `cache::upload_worker`, `result` ok/skipped/err       |
| `mammoth_bucket_uploads_dropped_total`  | yes, **not in 0004**         | `cache::enqueue_upload`, `reason` queue-full/shutdown |
| `mammoth_shutting_down`                 | yes (#39), **not in 0004**   | `Cache::shutdown`                                     |
| `mammoth_tls_*` (three, 0014)           | yes (#40), **not in 0004**   | `server`, `tls`                                       |
| `mammoth_cache_lookups_total`           | no                           | `Cache::get` keeps `hits`/`misses` atomics            |
| `mammoth_cache_stores_total`            | no                           | `Cache::put` knows every outcome                      |
| `mammoth_cache_entry_size_bytes`        | no                           | `Cache::put` has the final size                       |
| `mammoth_tier_reads_total`              | no                           | `Cache::get` loop sees hit, miss and error            |
| `mammoth_tier_read_duration_seconds`    | no                           | same loop, time to `store.get` returning              |
| `mammoth_tier_writes_total`             | no                           | every `store.put` in the core                         |
| `mammoth_tier_write_duration_seconds`   | no                           | same                                                  |
| `mammoth_tier_read_bytes_total`         | no                           | `Blob::size` on a hit                                 |
| `mammoth_tier_written_bytes_total`      | no                           | `put` returns the size written                        |
| `mammoth_tier_entries`                  | no                           | `BoundedStore::usage()` (serverStatus has it)         |
| `mammoth_tier_size_bytes`               | no                           | `usage().weight`                                      |
| `mammoth_tier_capacity_bytes`           | no                           | `usage().capacity`                                    |
| `mammoth_tier_oldest_entry_age_seconds` | no                           | nothing: needs a store hook                           |
| `mammoth_disk_free_bytes`               | yes (#39)                    | disk probe, `DiskStore::open` and every few seconds   |
| `mammoth_http_request_duration_seconds` | no                           | `http::metrics::record` wraps the handler             |
| `mammoth_http_requests_in_flight`       | no                           | same middleware                                       |
| `mammoth_auth_failures_total`           | no                           | `ApiError::reason()` on 401/403                       |
| `mammoth_build_info`                    | no                           | `CARGO_PKG_VERSION`; no revision yet                  |
| `process_*`                             | no                           | `metrics-process` crate                               |

Also missing: `HELP` text (`metrics::describe_*`), histogram bucket boundaries (0004 lists them as
an open question), and `/metrics` is only rendered when `main` installs the recorder, so the router
tests see `501`.

`serverStatus` reads `Cache`'s own `hits`/`misses` atomics. 0004 wants it to "read the same values
as the gauges and counters". The `metrics` facade cannot read a counter back, so the rule becomes:
the atomics and the counters are incremented by the same function at the same site, and the
hermetic test asserts they agree.

### Catalog changes

Additive. The first two land with this doc; the rest with the first instrumentation pull request:

- Add `mammoth_bucket_uploads_total{result="ok"|"skipped"|"error"}`,
  `mammoth_bucket_uploads_dropped_total{reason}` and `mammoth_shutting_down` under "Per tier", and
  the three `mammoth_tls_*` metrics from 0014. All already ship.
- Record the `metrics` facade as the implementation and close that open question.
- Note that `mammoth_tier_evictions_total{reason="age"}` has no producer until a TTL exists.
- Fix the histogram bucket boundaries:

  | Histogram                               | Buckets                                                                             |
  | --------------------------------------- | ----------------------------------------------------------------------------------- |
  | `mammoth_http_request_duration_seconds` | 1ms, 2.5ms, 5ms, 10ms, 25ms, 50ms, 100ms, 250ms, 500ms, 1s, 2.5s, 5s, 10s           |
  | `mammoth_tier_read_duration_seconds`    | 100µs, 250µs, 500µs, 1ms, 2.5ms, 5ms, 10ms, 25ms, 50ms, 100ms, 250ms, 500ms, 1s     |
  | `mammoth_tier_write_duration_seconds`   | same as reads, plus 2.5s, 5s                                                        |
  | `mammoth_cache_entry_size_bytes`        | 1 KiB, 4 KiB, 16 KiB, 64 KiB, 256 KiB, 1 MiB, 4 MiB, 16 MiB, 64 MiB, 256 MiB, 1 GiB |

  Set once with `PrometheusBuilder::set_buckets_for_metric` where the recorder is built.

### Instrumentation plan

Five small pull requests on `main`, in this order. Each one adds its metrics to the hermetic
test's catalog table and flips the docs note in `docs/user/metrics.md`. The framework thread owns
all five.

1. **Recorder and HELP.** Move recorder construction into a `telemetry` module: install the
   recorder, set the buckets, call `describe_*` for every catalogued metric, record
   `mammoth_build_info` (version from Cargo, revision from `MAMMOTH_REVISION` at build time, which
   the release workflow sets, and `unknown` otherwise), and add `metrics-process` for `process_*`.
   Tests get a `telemetry::test_recorder()` that installs once per process behind a `OnceLock` and
   returns the handle.
2. **Cache outcomes.** In `Cache::get`, `head` and `put`: `mammoth_cache_lookups_total`,
   `mammoth_cache_stores_total`, `mammoth_cache_entry_size_bytes`. The `hits`/`misses`/`oversized`
   atomics move behind the same helper so `serverStatus` cannot disagree.
3. **Tier reads and writes.** Wrap each `TierHandle`'s store call in the core with a timer:
   `mammoth_tier_reads_total`, `mammoth_tier_read_duration_seconds` (time until `store.get`
   returns the stream head, which is time to first byte), `mammoth_tier_read_bytes_total`,
   `mammoth_tier_writes_total`, `mammoth_tier_write_duration_seconds`,
   `mammoth_tier_written_bytes_total`. Promotions and uploads go through the same wrapper, so the
   bucket's writes are counted even though they are asynchronous.
4. **Population gauges.** After every local `put` and every eviction callback, set
   `mammoth_tier_entries`, `mammoth_tier_size_bytes` and `mammoth_tier_capacity_bytes` from
   `usage()`, which is O(1). `mammoth_disk_free_bytes` already ships from the disk probe (#39).
   `mammoth_tier_oldest_entry_age_seconds` needs the policy's oldest key: foyer and the disk index
   both expose it, but the `MemoryBackend` trait does not, so this one lands last behind a trait
   method with a default of "not reported".
5. **Serving.** `mammoth_http_request_duration_seconds` and `mammoth_http_requests_in_flight` in
   `http::metrics::record` (a guard decrements the gauge when the response body is dropped, so a
   client that walks away is still counted out). `mammoth_auth_failures_total{reason}` where
   `ApiError` is built for 401 and 403; `reason` is the AIP-193 reason, which is a fixed set.

### Hermetic tests

`crates/mammoth-cache/tests/metrics.rs` is already its own binary because the recorder is
process-wide. It grows into the catalog gate:

- A `CATALOG` table in the test lists every metric in 0004 with its type, label keys, the allowed
  values of each bounded label, and whether it is live on `main`. Each instrumentation pull request
  flips rows to live.
- One workload drives the router with `tower::ServiceExt::oneshot`: memory and disk tiers from
  `tempfile`, a `memory://` bucket, a store, a hit from each tier, a miss, an oversized drop, an
  unauthenticated request, a Gradle and a Maven `User-Agent`.
- After the workload the test scrapes `/metrics` and parses the exposition with a small parser in
  the test (name, labels, value, plus `# TYPE` lines). It asserts:
  - every live catalogued metric appears, with exactly the catalogued label keys and the right
    `# TYPE`;
  - no `mammoth_` family appears that the catalog does not list (the drift gate 0004 lacks);
  - every bounded label value is in its allowed set, and `route` values are templates (contain
    `{` or are `/metrics`, `/readyz` and friends), never a key;
  - hit and miss counters equal `GET /v1/serverStatus`;
  - the exposition round-trips `prometheus_client`'s rules: `_total` on counters, `_bucket`,
    `_sum`, `_count` present for every histogram;
  - every catalogued name appears in `docs/design/0004-metrics.md` and `docs/user/metrics.md`,
    via `include_str!`, so the docs cannot fall behind the table.
- Counter assertions use deltas between two scrapes, never absolute values, because other tests in
  the same binary share the recorder.

`promtool check metrics` stays an end-to-end check: it needs the Prometheus image. The in-test
parser covers syntax; promtool covers Prometheus's lint rules.

### End-to-end checks

The harness in pull request #27 already has what the E2E thread needs: `mammoth_e2e.metrics`
parses `/metrics` and takes counter deltas, `mammoth_e2e.prom` queries the Prometheus HTTP API,
`test_ops.py` runs `promtool check metrics` and checks the scrape target is up, and the 0011 test
table already keys round-trip, write-through and tier fall-through on counters. What to add once
the metrics exist, in the E2E thread's own order:

| Check            | Drive                                            | Assert                                                                |
| ---------------- | ------------------------------------------------ | --------------------------------------------------------------------- |
| Catalog present  | Any traffic, then scrape                         | Every live name from 0004's tables (parsed from the doc) is present   |
| Hit rate query   | N stores, N hits, M misses through Gradle client | `sum(rate(...hit))/sum(rate(...))` in Prometheus is N/(N+M) ±1 scrape |
| Tier of a hit    | Fill past memory, GET the oldest                 | `mammoth_cache_lookups_total{tier="disk"}` and `{tier="bucket"}` move |
| Write-through    | PUT, poll `mammoth_bucket_upload_queue_entries`  | Reaches 0; `mammoth_bucket_uploads_total{result="ok"}` += 1           |
| Client labels    | Gradle 8.x build, Maven build, curl              | `client` is `gradle`, `maven`, `other`; `gradle_version` is `8.x`     |
| Auth             | Wrong token, right token without scope           | `mammoth_auth_failures_total{reason=...}` += 1 each                   |
| Population       | Known number of stores                           | `mammoth_tier_entries{tier="memory"}` equals serverStatus's count     |
| Build info       | Scrape                                           | `version` equals the image tag under test                             |
| Process metrics  | Scrape                                           | `process_resident_memory_bytes` present and positive                  |
| Protected scrape | `metrics.require_auth = true` profile            | 401 without the token; Prometheus target up with `credentials_file`   |

The k6 load profiles from 0011 use the same series for their thresholds, so the load stage gets its
hit-rate and latency assertions for free once `mammoth_cache_lookups_total` and
`mammoth_http_request_duration_seconds` exist.

### Alerting

The node does not alert. 0004's rule that the node holds raw numbers and nothing else applies to
alerting as much as to rates: alert evaluation is state, history and a notification path, which
Prometheus and Alertmanager already do and every operator already runs. The node's job is to
expose the signals an alert needs, and the docs ship the rules.

The signals that are missing today are the bucket's. Persona audit #4 found that a bucket outage
shows up nowhere: `Cache::get` logs a warning and treats the error as a miss, and background
uploads log and count only in `mammoth_bucket_uploads_total{result="error"}`. After
instrumentation pull request 3, `mammoth_tier_reads_total{tier="bucket",result="error"}` and
`mammoth_tier_writes_total{tier="bucket",result="error"}` carry the outage, and the read-duration
histogram carries a slow bucket. One gauge is added to 0004 so an outage is a level, not only a
rate:

| Metric            | Type  | Labels | Meaning                                                         |
| ----------------- | ----- | ------ | --------------------------------------------------------------- |
| `mammoth_tier_up` | gauge | `tier` | 1 after a tier's last operation succeeded, 0 after it failed    |

It is set from the same wrapper that counts reads and writes, so it costs nothing extra. A failed
tier does not fail `/readyz`: the node still serves from memory and disk, and
[0013](0013-deployment.md) keeps readiness about the node, not its dependencies.

`docs/user/metrics.md` gets an "Alerting" section with recommended rules operators paste into
Prometheus, written so a copy works with no edits:

| Alert                     | Expression (sketch)                                                             | For  |
| ------------------------- | ------------------------------------------------------------------------------- | ---- |
| `MammothDown`             | `up{job="mammoth"} == 0`                                                        | 1m   |
| `MammothBucketDown`       | `mammoth_tier_up{tier="bucket"} == 0`                                           | 5m   |
| `MammothBucketErrors`     | `rate(mammoth_tier_reads_total{tier="bucket",result="error"}[5m]) > 0`          | 10m  |
| `MammothUploadsDropped`   | `increase(mammoth_bucket_uploads_dropped_total[10m]) > 0`                       | 0m   |
| `MammothUploadQueueStuck` | `mammoth_bucket_upload_queue_entries > 0` and not decreasing over 15m           | 15m  |
| `MammothIntegrityErrors`  | `increase(mammoth_cache_integrity_errors_total[1h]) > 0`                        | 0m   |
| `MammothDiskFull`         | `mammoth_disk_free_bytes < 1 GiB`                                               | 10m  |
| `MammothHitRateLow`       | hit-rate query from 0004 `< 0.5`                                                | 1h   |
| `MammothSlowRequests`     | `histogram_quantile(0.99, ... mammoth_http_request_duration_seconds ...) > 1`   | 15m  |
| `MammothAuthFailures`     | `rate(mammoth_auth_failures_total[5m]) > 1`                                     | 15m  |
| `MammothOversizedStores`  | `rate(mammoth_cache_stores_total{result="too_large"}[1h]) > 0`                  | 1h   |

Thresholds are starting points, said so in the docs. The end-to-end suite loads the same rules
into its Prometheus (0011 already runs one) and checks that `MammothBucketDown` fires when the
SeaweedFS container is stopped, which is also the test for the gauge.

Out of scope: webhooks, email or any notification path from the node, and health-check failure on
a bucket outage.

### Who does what

| Piece                                        | Owner               | Where                                   |
| -------------------------------------------- | ------------------- | --------------------------------------- |
| Instrumentation pull requests 1 to 5         | framework thread    | `crates/mammoth-cache/src`              |
| 0004 bucket boundaries and `age` note        | framework thread    | in pull request 1                       |
| Hermetic catalog gate, parser, docs check    | this thread         | `crates/mammoth-cache/tests/metrics.rs` |
| End-to-end checks above                      | E2E thread          | `tests/e2e`, after #27                  |
| `docs/user/metrics.md` "exported today" note | whoever flips a row | same pull request                       |

## Cross-cutting concerns

### Performance

Every new metric is one atomic operation per event. Durations use `Instant::now()` twice per tier
call. Gauges are set from O(1) `usage()` after writes, never by scanning. The in-flight gauge costs
one increment and one decrement per request.

### Security

Nothing new is exposed: every label value is from a fixed set or normalised, as `http::metrics`
already does for `client` and `gradle_version`. `reason` on auth failures is the AIP-193 reason,
not the user name.

## Alternatives considered

- **Switch to `prometheus-client` now**, as 0004 leaned. Typed label sets would make the drift gate
  a compile error. Rejected: `main` already records through the facade in four modules, the gate
  below catches the same drift at test time, and the facade keeps the exporter swappable.
- **Compute `serverStatus` from the recorder** so the two cannot disagree. The facade has no read
  path; `PrometheusHandle::render` then re-parsing is silly. Rejected in favour of one increment
  site plus a test.
- **Parse 0004's markdown tables in the Rust test** instead of a `CATALOG` table. Rejected: a
  Rust table gives label value sets and liveness, which the doc does not carry, and `include_str!`
  name checks keep the doc honest with far less fragility.

## Open questions

Decided 2026-10-14: `mammoth_build_info{revision}` is stamped by the release workflow through
`MAMMOTH_REVISION`, and local builds say `unknown`. A `build.rs` running `git rev-parse` would rerun
on every commit, and finds no checkout in the Docker and Bazel sandboxes anyway.

Decided 2026-10-14: `mammoth_tier_oldest_entry_age_seconds` ships for `disk` first, from the disk
index, and `memory` follows behind a `MemoryBackend` trait method with a default of "not reported".
