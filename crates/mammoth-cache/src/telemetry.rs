//! The Prometheus recorder behind `/metrics` (design docs 0004 and 0015).
//!
//! [`install`] runs before anything records, so the gauges the tiers set while opening, such as
//! `mammoth_disk_free_bytes`, are not lost. It fixes the histogram buckets, gives every catalogued
//! metric its `HELP` text and records `mammoth_build_info`. `process_*` metrics are collected on
//! each scrape by [`render`].

use std::sync::OnceLock;

use metrics::{describe_counter, describe_gauge, describe_histogram, gauge};
use metrics_exporter_prometheus::{BuildError, Matcher, PrometheusBuilder, PrometheusHandle};

use crate::cache::CacheStatus;

/// The commit the binary was built from, from `MAMMOTH_REVISION` at build time.
const REVISION: &str = match option_env!("MAMMOTH_REVISION") {
    Some(revision) => revision,
    None => "unknown",
};

const MS: f64 = 1e-3;
const KIB: f64 = 1024.0;
const MIB: f64 = 1024.0 * KIB;

/// Bucket boundaries from 0015, per histogram.
fn buckets() -> [(&'static str, Vec<f64>); 5] {
    let tier_reads = vec![
        0.1 * MS,
        0.25 * MS,
        0.5 * MS,
        MS,
        2.5 * MS,
        5.0 * MS,
        10.0 * MS,
        25.0 * MS,
        50.0 * MS,
        100.0 * MS,
        250.0 * MS,
        500.0 * MS,
        1.0,
    ];
    let mut tier_writes = tier_reads.clone();
    tier_writes.extend([2.5, 5.0]);
    let requests = vec![
        MS,
        2.5 * MS,
        5.0 * MS,
        10.0 * MS,
        25.0 * MS,
        50.0 * MS,
        100.0 * MS,
        250.0 * MS,
        500.0 * MS,
        1.0,
        2.5,
        5.0,
        10.0,
    ];
    // A gRPC call can stream a large blob, so its buckets go further.
    let mut calls = requests.clone();
    calls.extend([30.0, 60.0]);
    [
        ("mammoth_http_request_duration_seconds", requests),
        ("mammoth_grpc_request_duration_seconds", calls),
        ("mammoth_tier_read_duration_seconds", tier_reads),
        ("mammoth_tier_write_duration_seconds", tier_writes),
        (
            "mammoth_cache_entry_size_bytes",
            vec![
                KIB,
                4.0 * KIB,
                16.0 * KIB,
                64.0 * KIB,
                256.0 * KIB,
                MIB,
                4.0 * MIB,
                16.0 * MIB,
                64.0 * MIB,
                256.0 * MIB,
                1024.0 * MIB,
            ],
        ),
    ]
}

/// Installs the process-wide recorder. Fails if one is already installed.
///
/// # Errors
///
/// When a recorder is already installed, or a bucket list is empty.
pub fn install() -> Result<PrometheusHandle, BuildError> {
    let mut builder = PrometheusBuilder::new();
    for (name, buckets) in buckets() {
        builder = builder.set_buckets_for_metric(Matcher::Full(name.to_owned()), &buckets)?;
    }
    let handle = builder.install_recorder()?;
    describe();
    process().describe();
    gauge!(
        "mammoth_build_info",
        "version" => env!("CARGO_PKG_VERSION"),
        "revision" => REVISION,
    )
    .set(1.0);
    Ok(handle)
}

/// The recorder for tests: installed on first use, then shared by every test in the binary.
///
/// # Panics
///
/// When another recorder was installed first.
#[must_use]
pub fn test_recorder() -> PrometheusHandle {
    static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
    #[allow(clippy::expect_used)]
    HANDLE
        .get_or_init(|| install().expect("another metrics recorder is installed"))
        .clone()
}

/// The exposition for one scrape, with fresh `process_*` values.
#[must_use]
pub fn render(handle: &PrometheusHandle) -> String {
    render_with(handle, None)
}

/// [`render`], with the local tiers' population from `status` set first, so the gauges are as
/// current as the scrape.
#[must_use]
pub fn render_with(handle: &PrometheusHandle, status: Option<&CacheStatus>) -> String {
    if let Some(status) = status {
        record_population(status);
    }
    process().collect();
    crate::logging::record_dropped();
    handle.render()
}

#[allow(clippy::cast_precision_loss)] // Gauges; exact below 2^53.
fn record_population(status: &CacheStatus) {
    for (tier, usage) in &status.tiers {
        let tier = tier.as_str();
        gauge!("mammoth_tier_entries", "tier" => tier).set(usage.entries as f64);
        gauge!("mammoth_tier_size_bytes", "tier" => tier).set(usage.size_bytes as f64);
        gauge!("mammoth_tier_capacity_bytes", "tier" => tier).set(usage.capacity_bytes as f64);
    }
}

fn process() -> &'static metrics_process::Collector {
    static COLLECTOR: OnceLock<metrics_process::Collector> = OnceLock::new();
    COLLECTOR.get_or_init(metrics_process::Collector::default)
}

#[derive(Clone, Copy)]
enum Kind {
    Counter,
    Gauge,
    Histogram,
}

/// `HELP` text for every metric in 0004. Describing a metric does not export it: it appears once
/// something records it.
const HELP: &[(Kind, &str, &str)] = {
    use Kind::{Counter, Gauge, Histogram};
    &[
        // Cache outcomes
        (
            Counter,
            "mammoth_cache_lookups_total",
            "Loads by outcome; tier is the tier that served a hit.",
        ),
        (
            Counter,
            "mammoth_cache_stores_total",
            "Stores by outcome; too_large entries were accepted and dropped, client_error \
             uploads failed on the client's side.",
        ),
        (
            Histogram,
            "mammoth_cache_entry_size_bytes",
            "Size of each stored entry, in bytes.",
        ),
        (
            Counter,
            "mammoth_cache_integrity_errors_total",
            "Entries whose checksum did not match; served as misses.",
        ),
        // Per tier
        (Counter, "mammoth_tier_reads_total", "Reads against a tier."),
        (
            Histogram,
            "mammoth_tier_read_duration_seconds",
            "Time for a tier to answer a read.",
        ),
        (Counter, "mammoth_tier_writes_total", "Writes to a tier."),
        (
            Histogram,
            "mammoth_tier_write_duration_seconds",
            "Time to write an entry to a tier.",
        ),
        (
            Counter,
            "mammoth_tier_read_bytes_total",
            "Bytes read from a tier.",
        ),
        (
            Counter,
            "mammoth_tier_written_bytes_total",
            "Bytes written to a tier.",
        ),
        (
            Counter,
            "mammoth_tier_promotions_total",
            "Entries copied up a tier after a hit.",
        ),
        (
            Counter,
            "mammoth_tier_evictions_total",
            "Entries removed to stay within limits.",
        ),
        (
            Gauge,
            "mammoth_bucket_upload_queue_entries",
            "Stores waiting to be uploaded to the bucket.",
        ),
        (
            Counter,
            "mammoth_bucket_uploads_total",
            "Background uploads by outcome; skipped entries left the local tiers first.",
        ),
        (
            Counter,
            "mammoth_bucket_uploads_dropped_total",
            "Uploads skipped; the entry stays local.",
        ),
        (Gauge, "mammoth_shutting_down", "1 while the node drains."),
        (
            Gauge,
            "mammoth_tier_up",
            "1 after a tier's last operation succeeded, 0 after it failed.",
        ),
        // Population
        (
            Gauge,
            "mammoth_tier_entries",
            "Entries held, for memory and disk.",
        ),
        (
            Gauge,
            "mammoth_tier_size_bytes",
            "Bytes held, for memory and disk.",
        ),
        (
            Gauge,
            "mammoth_tier_capacity_bytes",
            "Configured limit in bytes, for memory and disk.",
        ),
        (
            Gauge,
            "mammoth_tier_oldest_entry_age_seconds",
            "Age of the least recently used entry.",
        ),
        (
            Gauge,
            "mammoth_disk_free_bytes",
            "Free space on the disk tier's file system.",
        ),
        // TLS
        (
            Gauge,
            "mammoth_tls_certificate_expiry_seconds",
            "When the serving certificate expires, in Unix seconds.",
        ),
        (
            Counter,
            "mammoth_tls_reloads_total",
            "Renewed certificates swapped in, or refused.",
        ),
        (
            Counter,
            "mammoth_tls_handshake_failures_total",
            "TLS handshakes that failed, such as plain HTTP on a TLS port.",
        ),
        // Serving
        (
            Counter,
            "mammoth_http_requests_total",
            "Requests by route template, status and client.",
        ),
        (
            Counter,
            "mammoth_gradle_requests_total",
            "Gradle requests by Gradle version.",
        ),
        (
            Histogram,
            "mammoth_http_request_duration_seconds",
            "Time to the last byte of the response.",
        ),
        (
            Gauge,
            "mammoth_http_requests_in_flight",
            "Requests being served.",
        ),
        (
            Counter,
            "mammoth_grpc_requests_total",
            "REAPI gRPC calls by method and status code, refused ones included.",
        ),
        (
            Histogram,
            "mammoth_grpc_request_duration_seconds",
            "Time from a gRPC call's start to its status.",
        ),
        (
            Gauge,
            "mammoth_grpc_requests_in_flight",
            "gRPC calls being served.",
        ),
        (
            Counter,
            "mammoth_auth_failures_total",
            "Rejected credentials, by AIP-193 reason.",
        ),
        (
            Counter,
            "mammoth_log_lines_dropped_total",
            "Log lines dropped because stdout could not keep up.",
        ),
        (
            Gauge,
            "mammoth_build_info",
            "Always 1; identifies the build.",
        ),
    ]
};

fn describe() {
    for &(kind, name, help) in HELP {
        match kind {
            Kind::Counter => describe_counter!(name, help),
            Kind::Gauge => describe_gauge!(name, help),
            Kind::Histogram => describe_histogram!(name, help),
        }
    }
}
