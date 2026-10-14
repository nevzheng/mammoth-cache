# 0004: Metrics

## Status

Draft

## Summary

mammoth-cache exports Prometheus metrics at `/metrics`. This doc is the catalog: each metric's name,
type, labels and meaning. The catalog is a contract like the APIs: renaming or removing a metric,
or changing its labels, is a breaking change. The admin API's `serverStatus` returns the same
current-population numbers as a snapshot.

The node's metrics are deliberately dumb: it exposes raw counters, gauges and histograms and
nothing else. Rates, ratios, rollups, history and reporting are the job of an external
aggregator such as Prometheus.

## Context and scope

Operators need to know whether the cache is helping (hit rate), where time goes (which tier), and
whether it is healthy (size, evictions, errors). The tiers are described in
[0003](0003-architecture.md). Prometheus has no schema language, so this catalog is the schema.

## Goals and non-goals

### Goals

- Give an aggregator what it needs to answer "is the cache working?": hit rate, latency per tier,
  population.
- Keep the node's part trivial: increment, set, observe.
- Follow Prometheus naming: `snake_case`, base units (seconds, bytes), `_total` on counters.
- Bounded label cardinality.

### Non-goals

- Aggregation, rollups, rates, time windows, history, dashboards or reports in the node. External
  aggregators do this better, and keeping it out keeps the node stateless.
- Per-key or per-build statistics. Build analytics belong to build tools, not the cache.
- Tracing. Possible later with OpenTelemetry.

## Design

### Conventions

- Every metric starts with `mammoth_`.
- `tier` is one of `memory`, `disk`, `bucket`.
- No label ever holds a cache key, a token or any other unbounded value.
- Only raw values are exported: monotonic counters, current gauges and histograms. Hit rate,
  request rates and averages are computed by the aggregator, never exported.
- Histograms, never summaries: histogram buckets can be summed across nodes, while precomputed
  quantiles cannot.
- Histograms use base units; bucket boundaries are chosen per metric and listed in the user guide.

### Catalog

#### Cache outcomes

| Metric                                 | Type      | Labels                                                              | Meaning                                                                                                              |
| -------------------------------------- | --------- | ------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------- |
| `mammoth_cache_lookups_total`          | counter   | `result` (`hit`, `miss`), `tier`                                    | Loads by outcome; `tier` is the tier that served a hit, empty on a miss                                              |
| `mammoth_cache_stores_total`           | counter   | `result` (`stored`, `exists`, `too_large`, `client_error`, `error`) | Stores by outcome; `too_large` entries were accepted and dropped; `client_error` uploads failed on the client's side |
| `mammoth_cache_entry_size_bytes`       | histogram |                                                                     | Size of each stored entry                                                                                            |
| `mammoth_cache_integrity_errors_total` | counter   | `tier`                                                              | Entries whose checksum did not match; served as misses                                                               |

#### Per tier

| Metric                                 | Type      | Labels                                             | Meaning                                                             |
| -------------------------------------- | --------- | -------------------------------------------------- | ------------------------------------------------------------------- |
| `mammoth_tier_reads_total`             | counter   | `tier`, `result` (`hit`, `miss`, `error`)          | Reads against a tier                                                |
| `mammoth_tier_read_duration_seconds`   | histogram | `tier`                                             | Time to first byte from a tier                                      |
| `mammoth_tier_writes_total`            | counter   | `tier`, `result` (`ok`, `error`)                   | Writes to a tier                                                    |
| `mammoth_tier_write_duration_seconds`  | histogram | `tier`                                             | Time to write an entry to a tier                                    |
| `mammoth_tier_read_bytes_total`        | counter   | `tier`                                             | Bytes read from a tier                                              |
| `mammoth_tier_written_bytes_total`     | counter   | `tier`                                             | Bytes written to a tier                                             |
| `mammoth_tier_promotions_total`        | counter   | `from`, `to`                                       | Entries copied up a tier after a hit                                |
| `mammoth_tier_evictions_total`         | counter   | `tier`, `reason` (`size`, `age`)                   | Entries removed to stay within limits                               |
| `mammoth_bucket_upload_queue_entries`  | gauge     |                                                    | Stores waiting to be uploaded to the bucket                         |
| `mammoth_bucket_uploads_total`         | counter   | `result` (`ok`, `skipped`, `error`)                | Background uploads by outcome; `skipped` left the local tiers first |
| `mammoth_bucket_uploads_dropped_total` | counter   | `reason` (`queue-full`, `shutdown`, `bucket-down`) | Uploads skipped; the entry stays local                              |
| `mammoth_shutting_down`                | gauge     |                                                    | 1 while the node drains ([0013](0013-deployment.md))                |
| `mammoth_tier_up`                      | gauge     | `tier`                                             | 1 after a tier's last operation succeeded, 0 after it failed        |

#### Current population

| Metric                                  | Type  | Labels | Meaning                                                     |
| --------------------------------------- | ----- | ------ | ----------------------------------------------------------- |
| `mammoth_tier_entries`                  | gauge | `tier` | Entries held, for `memory` and `disk`                       |
| `mammoth_tier_size_bytes`               | gauge | `tier` | Bytes held, for `memory` and `disk`                         |
| `mammoth_tier_capacity_bytes`           | gauge | `tier` | Configured limit, for `memory` and `disk`                   |
| `mammoth_tier_oldest_entry_age_seconds` | gauge | `tier` | Age of the least recently used entry: how much history fits |
| `mammoth_disk_free_bytes`               | gauge |        | Free space on the disk tier's file system                   |

The bucket's population is not reported: counting it means listing the bucket, which is slow and
costs money. The provider's own metrics cover it.

#### TLS

Only with `[tls]` set; defined in [0014](0014-tls.md).

| Metric                                   | Type    | Labels                        | Meaning                                      |
| ---------------------------------------- | ------- | ----------------------------- | -------------------------------------------- |
| `mammoth_tls_certificate_expiry_seconds` | gauge   |                               | When the serving certificate expires, Unix s |
| `mammoth_tls_reloads_total`              | counter | `result` (`ok`, `error`)      | Renewed certificates swapped in, or refused  |
| `mammoth_tls_handshake_failures_total`   | counter | `reason` (`error`, `timeout`) | Handshakes that failed, such as plain HTTP   |

#### Serving

| Metric                                  | Type      | Labels                              | Meaning                                       |
| --------------------------------------- | --------- | ----------------------------------- | --------------------------------------------- |
| `mammoth_http_requests_total`           | counter   | `route`, `method`, `code`, `client` | Requests by route template, status and client |
| `mammoth_gradle_requests_total`         | counter   | `gradle_version`                    | Gradle requests by Gradle version             |
| `mammoth_grpc_requests_total`           | counter   | `method`, `code`                    | REAPI gRPC calls by method and status code    |
| `mammoth_http_request_duration_seconds` | histogram | `route`, `method`                   | Time to the last byte of the response         |
| `mammoth_http_requests_in_flight`       | gauge     | `route`                             | Requests being served                         |
| `mammoth_auth_failures_total`           | counter   | `reason`                            | Rejected credentials, by AIP-193 `reason`     |
| `mammoth_build_info`                    | gauge     | `version`, `revision`               | Always 1; identifies the build                |

`route` is the route template (`/cache/{key}`), never the requested path. `client` is `gradle`,
`maven` or `other`, from the `User-Agent` prefix (`Gradle/`, `Apache-Maven/`). `gradle_version` is
the major and minor version from `X-Gradle-Version` (`8.10`), or `other` if it does not parse.
Neither is trusted: clients choose them, so they label metrics and logs and nothing else, and there
is no list of supported clients. Standard `process_*`
metrics (memory, CPU, file descriptors) are exported too.

### Example queries

```promql
# Hit rate over 5 minutes
sum(rate(mammoth_cache_lookups_total{result="hit"}[5m]))
  / sum(rate(mammoth_cache_lookups_total[5m]))

# Share of hits served from memory
sum(rate(mammoth_cache_lookups_total{result="hit",tier="memory"}[5m]))
  / sum(rate(mammoth_cache_lookups_total{result="hit"}[5m]))

# p99 time to first byte from the bucket
histogram_quantile(0.99, sum by (le) (rate(mammoth_tier_read_duration_seconds_bucket{tier="bucket"}[5m])))
```

### Alerting

The node does not alert, for the same reason it does not aggregate: rules, state and notification
belong to Prometheus and Alertmanager. The user guide ships recommended alert rules over this
catalog. A tier outage, the bucket's above all, is visible as `mammoth_tier_up{tier}` and as
`result="error"` on that tier's reads and writes; it does not fail `/readyz`, which is about the
node ([0013](0013-deployment.md)).

### Admin snapshot

`GET /v1/serverStatus` returns the current population per tier and lifetime hit and miss counts, for
scripts and `mammoth status`; there is no web UI ([0003](0003-architecture.md)). Like the metrics,
these are raw values with no rates or ratios. It reads the same values as the gauges and counters
above, so the two never disagree.

### Verification

A hermetic test drives the router through every path that records a metric, scrapes `/metrics` and
checks the exposition against this catalog: every live metric present with its type and labels, no
metric outside the catalog, every bounded label value in its set. The end-to-end suite
([0011](https://github.com/nevzheng/mammoth-cache/pull/27)) scrapes the real image through a real
Prometheus and checks it with `promtool check metrics`.

### Implementation

No database or separate in-memory store is needed. Every metric is a number held in process
memory: a counter or gauge is one atomic integer, and a histogram is a fixed array of atomic bucket
counters plus a sum and a count. Memory is set by the catalog and label cardinality, not by
traffic. A scrape of `/metrics` reads the current values and renders them as Prometheus text; the
node keeps no history, and a restart resets counters to zero, which Prometheus's `rate()` handles.

The node records through the [`metrics`](https://crates.io/crates/metrics) facade and renders
`/metrics` with `metrics-exporter-prometheus`. The facade keeps the stores free of any exporter
type and allows a different backend later, at the cost of a global registry and string-keyed
metrics. Because the names are strings, the catalog test in
`crates/mammoth-cache/tests/metrics.rs` is what holds the code to this doc: it fails on a
`mammoth_` metric the catalog does not list, on a wrong type or label set, and on a bounded label
value outside its set. The alternative was
[`prometheus-client`](https://crates.io/crates/prometheus-client), whose typed label sets would
catch the same drift at compile time; it lost because every store already recorded through the
facade when `/metrics` was wired up.

Rollout order, the hermetic test and the end-to-end checks are in
[0015](0015-metrics-rollout-and-testing.md).

### Namespace and access

- **Namespace:** every metric name starts with `mammoth_`, so the node's metrics never collide with
  others in the same Prometheus. The standard `process_*` metrics keep their usual names.
- **Route:** `/metrics`, on the main listener, outside the `/cache/`, `/maven/` and `/v1/` routes.
- **Access:** open by default, like the Build Cache Node's metrics and most Prometheus exporters.
  With `[metrics] require_auth = true` ([0005](0005-configuration.md)), `/metrics` needs a Bearer
  token whose user has the `metrics:read` scope ([0002](0002-authentication.md)). Prometheus sends
  it with `authorization` and `credentials_file` in its scrape config. A scraper gets its own user
  with only `metrics:read`, so its token cannot read or write the cache.

## Cross-cutting concerns

### Performance

Counters and histograms are updated without locks on the request path. Gauges for population are
maintained incrementally, never computed by scanning a tier. Rendering a scrape is proportional to
the number of series, which the bounded labels keep small.

### Security

See [Namespace and access](#namespace-and-access). No metric exposes keys, tokens or user names.

## Alternatives considered

- **OpenTelemetry metrics only:** a broader standard, but Prometheus scraping is what most
  operators run, and an OpenTelemetry collector can scrape Prometheus endpoints.
- **Per-user labels:** useful for showing who pushes what, but the user list is unbounded in
  principle. Can be added behind a setting if needed.

## Open questions

- Histogram bucket boundaries for each duration and size metric
  ([0015](0015-metrics-rollout-and-testing.md) proposes them).
- Whether to export per-user request counts behind a setting.
