# Metrics

> [!NOTE]
> Exported today: `mammoth_http_requests_total`, `mammoth_gradle_requests_total`,
> `mammoth_http_request_duration_seconds`, `mammoth_http_requests_in_flight`,
> `mammoth_grpc_requests_total`, `mammoth_auth_failures_total`, `mammoth_cache_lookups_total`,
> `mammoth_cache_stores_total`, `mammoth_cache_entry_size_bytes`, `mammoth_tier_promotions_total`,
> `mammoth_tier_evictions_total` (memory tier), `mammoth_cache_integrity_errors_total` (disk tier),
> `mammoth_bucket_upload_queue_entries`, `mammoth_bucket_uploads_total`,
> `mammoth_bucket_uploads_dropped_total`, `mammoth_shutting_down`, `mammoth_disk_free_bytes`,
> `mammoth_tier_entries`, `mammoth_tier_size_bytes`, `mammoth_tier_capacity_bytes`,
> `mammoth_build_info`, the `mammoth_tls_*` metrics, `mammoth_tier_up`, the per-tier reads, writes,
> durations and bytes, and the standard `process_*` metrics. The rest of the catalog below is the
> contract from design doc [0004](../design/0004-metrics.md) and arrives with the features it
> measures. Names and labels won't change without a breaking release.

mammoth-cache exports Prometheus metrics at `/metrics` on the main port. The node exports raw
counters, gauges and histograms only. Hit rates, request rates and dashboards are computed by
Prometheus, so the node keeps no history and a restart simply resets counters.

## Scraping

Metrics are open by default, like the Build Cache Node's:

```yaml
scrape_configs:
  - job_name: mammoth
    static_configs:
      - targets: ["cache.example.com:5071"]
```

With `[metrics] require_auth = true`, give Prometheus its own user with only the `metrics:read`
scope (see [Authentication](authentication.md)), so its token can't read or write the cache:

```yaml
    authorization:
      credentials_file: /etc/prometheus/mammoth-token
```

## Catalog

Every metric starts with `mammoth_`. `tier` is `memory`, `disk` or `bucket`. No label ever holds a
cache key, a token or a user name.

### Cache outcomes

| Metric                                 | Type      | Labels                                                              | Meaning                                                                                                                        |
| -------------------------------------- | --------- | ------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| `mammoth_cache_lookups_total`          | counter   | `result` (`hit`, `miss`), `tier`                                    | Loads by outcome; `tier` served the hit, empty on a miss                                                                       |
| `mammoth_cache_stores_total`           | counter   | `result` (`stored`, `exists`, `too_large`, `client_error`, `error`) | Stores by outcome; `too_large` were accepted and dropped; `client_error` failed on the client's side, such as a dropped upload |
| `mammoth_cache_entry_size_bytes`       | histogram |                                                                     | Size of each stored entry                                                                                                      |
| `mammoth_cache_integrity_errors_total` | counter   | `tier`                                                              | Entries whose checksum didn't match; served as misses                                                                          |

### Per tier

| Metric                                 | Type      | Labels                                             | Meaning                                          |
| -------------------------------------- | --------- | -------------------------------------------------- | ------------------------------------------------ |
| `mammoth_tier_reads_total`             | counter   | `tier`, `result` (`hit`, `miss`, `error`)          | Reads against a tier                             |
| `mammoth_tier_read_duration_seconds`   | histogram | `tier`                                             | Time for a tier to answer a read                 |
| `mammoth_tier_writes_total`            | counter   | `tier`, `result` (`ok`, `error`)                   | Writes to a tier                                 |
| `mammoth_tier_write_duration_seconds`  | histogram | `tier`                                             | Time to write an entry to a tier                 |
| `mammoth_tier_read_bytes_total`        | counter   | `tier`                                             | Bytes read from a tier                           |
| `mammoth_tier_written_bytes_total`     | counter   | `tier`                                             | Bytes written to a tier                          |
| `mammoth_tier_promotions_total`        | counter   | `from`, `to`                                       | Entries copied up a tier after a hit             |
| `mammoth_tier_evictions_total`         | counter   | `tier`, `reason` (`size`, `age`)                   | Entries removed to stay within limits            |
| `mammoth_bucket_upload_queue_entries`  | gauge     |                                                    | Stores waiting to upload to the bucket           |
| `mammoth_bucket_uploads_total`         | counter   | `result` (`ok`, `skipped`, `error`)                | Uploads to the bucket, sync or background        |
| `mammoth_bucket_uploads_dropped_total` | counter   | `reason` (`queue-full`, `shutdown`, `bucket-down`) | Uploads skipped; the entry stays local           |
| `mammoth_tier_up`                      | gauge     | `tier` (`bucket`)                                  | `0` from a bucket failure until it answers again |
| `mammoth_shutting_down`                | gauge     |                                                    | `0`, then `1` while the node drains              |

A read is a lookup or a size check. A hit's read duration is the time to its first byte; its bytes
are counted as they stream. Writes include promotions and bucket uploads. A write that fails because
of its body, such as a client that disconnects mid-upload, is not a tier error and is not counted.

### Current population

| Metric                                  | Type  | Labels | Meaning                                                     |
| --------------------------------------- | ----- | ------ | ----------------------------------------------------------- |
| `mammoth_tier_entries`                  | gauge | `tier` | Entries held, for `memory` and `disk`                       |
| `mammoth_tier_size_bytes`               | gauge | `tier` | Bytes held, for `memory` and `disk`                         |
| `mammoth_tier_capacity_bytes`           | gauge | `tier` | Configured limit, for `memory` and `disk`                   |
| `mammoth_tier_oldest_entry_age_seconds` | gauge | `tier` | Age of the least recently used entry: how much history fits |
| `mammoth_disk_free_bytes`               | gauge |        | Free space on the disk tier's file system                   |

The population gauges are read when Prometheus scrapes, so they are as current as the scrape.
`mammoth_tier_oldest_entry_age_seconds` is not exported yet. The bucket's population isn't reported,
because counting it means listing the bucket. Use the cloud provider's metrics.

### Serving

| Metric                                  | Type      | Labels                              | Meaning                                |
| --------------------------------------- | --------- | ----------------------------------- | -------------------------------------- |
| `mammoth_http_requests_total`           | counter   | `route`, `method`, `code`, `client` | Requests by route, status and client   |
| `mammoth_gradle_requests_total`         | counter   | `gradle_version`                    | Gradle requests by major.minor version |
| `mammoth_grpc_requests_total`           | counter   | `method`, `code`                    | REAPI gRPC calls by method and status  |
| `mammoth_http_request_duration_seconds` | histogram | `route`, `method`                   | Time to the last byte of the response  |
| `mammoth_http_requests_in_flight`       | gauge     | `route`                             | Requests being served                  |
| `mammoth_auth_failures_total`           | counter   | `reason`                            | Rejected credentials, by `reason`      |
| `mammoth_build_info`                    | gauge     | `version`, `revision`               | Always 1; identifies the build         |

`route` is the route template, such as `/cache/{key}`. On the REAPI gRPC port, `method` is the gRPC
method, such as `FindMissingBlobs`, and `code` the gRPC status, such as `NOT_FOUND`. `client` is
`gradle`, `maven` or `other`, from the `User-Agent`. `gradle_version` is the major and minor version
from `X-Gradle-Version`, on `/cache/{key}` requests that passed authentication, or `other` if it
does not parse or is out of range (major 1 to 20, minor below 100). `revision` is the commit an
official build came from, and `unknown` for a build from source. The standard `process_*` metrics
(memory, CPU, file descriptors) are exported too.

### TLS

Only with [`[tls]`](configuration.md#tls) set.

| Metric                                   | Type    | Labels                          | Meaning                                       |
| ---------------------------------------- | ------- | ------------------------------- | --------------------------------------------- |
| `mammoth_tls_certificate_expiry_seconds` | gauge   |                                 | When the serving certificate expires, Unix s  |
| `mammoth_tls_reloads_total`              | counter | `result` (`ok`, `error`)        | Renewed certificates swapped in, or refused   |
| `mammoth_tls_handshake_failures_total`   | counter | `reason` (`error`, `timeout`)   | Handshakes that failed, such as plain HTTP    |

## Alerting

mammoth-cache does not send alerts; Prometheus and Alertmanager do. A bucket outage does not fail
`/readyz`, because the node keeps serving from memory and disk, so alert on
`mammoth_tier_up{tier="bucket"} == 0` and on `result="error"` for the bucket's reads and writes.
Recommended rules will be listed here as the metrics they need land.

## Useful queries

```promql
# Hit rate over 5 minutes
sum(rate(mammoth_cache_lookups_total{result="hit"}[5m]))
  / sum(rate(mammoth_cache_lookups_total[5m]))

# Share of hits served from memory
sum(rate(mammoth_cache_lookups_total{result="hit",tier="memory"}[5m]))
  / sum(rate(mammoth_cache_lookups_total{result="hit"}[5m]))

# Oversized stores dropped per second
sum(rate(mammoth_cache_stores_total{result="too_large"}[5m]))

# p99 time to first byte from the bucket
histogram_quantile(0.99, sum by (le) (rate(mammoth_tier_read_duration_seconds_bucket{tier="bucket"}[5m])))

# Days until the TLS certificate expires; alert below 14
(mammoth_tls_certificate_expiry_seconds - time()) / 86400
```

If `mammoth_tier_oldest_entry_age_seconds` for the disk is shorter than the life of a typical
feature branch, the disk is too small for your builds.
