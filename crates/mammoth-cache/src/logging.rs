//! Log output (design doc 0018): one JSON object per line on stdout, written by a background
//! thread so a slow log pipe never holds up a request.
//!
//! Each JSON line starts with `timestamp` (RFC 3339, UTC, microseconds), `severity` (Cloud
//! Logging's names: `DEBUG`, `INFO`, `WARNING`, `ERROR`), `target` and `message`, followed by the
//! event's own fields in the order they were recorded. Those names are what Google Cloud Logging,
//! Datadog and Loki read without configuration. Logging is best effort: when the buffer is full a
//! line is dropped and counted in `mammoth_log_lines_dropped_total`.

use std::{
    fmt::{self, Write as _},
    io,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::Value;
use tracing::{
    Event, Level, Subscriber,
    field::{Field, Visit},
};
use tracing_appender::non_blocking::{ErrorCounter, NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::{
    EnvFilter,
    fmt::{FmtContext, FormatEvent, FormatFields, format::Writer},
    registry::LookupSpan,
};

use crate::config::{Log, LogFormat};

/// Installs the global subscriber for `log`, writing to stdout through a buffer of
/// `log.buffer_lines` lines. The access log is written whatever `log.level` says, when
/// `log.access_log` is on. `RUST_LOG`, when set, replaces both.
///
/// Keep the guard until the process exits: dropping it flushes the lines still buffered.
pub fn init(log: &Log) -> WorkerGuard {
    let (writer, guard) = NonBlockingBuilder::default()
        .lossy(true)
        .buffered_lines_limit(log.buffer_lines)
        .thread_name("mammoth-log")
        .finish(io::stdout());
    let _ = DROPPED.set(writer.error_counter());
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let access = if log.access_log { "info" } else { "off" };
        format!("{},{}={access}", log.level.as_str(), crate::access::TARGET).into()
    });
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer);
    match log.format {
        LogFormat::Json => builder.event_format(JsonFormat).init(),
        LogFormat::Text => builder.init(),
    }
    guard
}

static DROPPED: OnceLock<ErrorCounter> = OnceLock::new();

/// Copies the count of dropped log lines into `mammoth_log_lines_dropped_total`, before a scrape.
/// It reads 0 when [`init`] was not called, as in tests.
pub(crate) fn record_dropped() {
    let dropped = DROPPED.get().map_or(0, ErrorCounter::dropped_lines);
    metrics::counter!("mammoth_log_lines_dropped_total").absolute(dropped as u64);
}

/// The JSON line format. Fields named like the envelope (`timestamp`, `severity`, `target`) are
/// written as `fields.<name>`, so a line never has a duplicate key.
#[derive(Debug, Clone, Copy, Default)]
pub struct JsonFormat;

impl<S, N> FormatEvent<S, N> for JsonFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        writer.write_str(&format_line(SystemTime::now(), event))
    }
}

/// One event as a JSON line, newline included.
fn format_line(now: SystemTime, event: &Event<'_>) -> String {
    let mut fields = Fields::default();
    event.record(&mut fields);
    let meta = event.metadata();
    let mut line = String::with_capacity(256);
    line.push('{');
    push_pair(&mut line, "timestamp", &Value::String(rfc3339_micros(now)));
    line.push(',');
    push_pair(&mut line, "severity", &Value::from(severity(*meta.level())));
    line.push(',');
    push_pair(&mut line, "target", &Value::from(meta.target()));
    line.push(',');
    push_pair(&mut line, "message", &Value::String(fields.message));
    for (name, value) in &fields.values {
        line.push(',');
        if matches!(*name, "timestamp" | "severity" | "target") {
            push_pair(&mut line, &format!("fields.{name}"), value);
        } else {
            push_pair(&mut line, name, value);
        }
    }
    line.push_str("}\n");
    line
}

fn push_pair(line: &mut String, key: &str, value: &Value) {
    // Serializing a string or a scalar cannot fail.
    line.push_str(&serde_json::to_string(key).unwrap_or_default());
    line.push(':');
    line.push_str(&serde_json::to_string(value).unwrap_or_default());
}

/// Cloud Logging's severity names, which Datadog and Loki also read. It has no trace level.
fn severity(level: Level) -> &'static str {
    match level {
        Level::ERROR => "ERROR",
        Level::WARN => "WARNING",
        Level::INFO => "INFO",
        Level::DEBUG | Level::TRACE => "DEBUG",
    }
}

/// An event's message and fields, as JSON values.
#[derive(Default)]
struct Fields {
    message: String,
    values: Vec<(&'static str, Value)>,
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let text = format!("{value:?}");
        if field.name() == "message" {
            self.message = text;
        } else {
            self.values.push((field.name(), Value::String(text)));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            value.clone_into(&mut self.message);
        } else {
            self.values.push((field.name(), Value::from(value)));
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.values.push((field.name(), Value::from(value)));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.values.push((field.name(), Value::from(value)));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        // NaN and infinities have no JSON number.
        let value = serde_json::Number::from_f64(value)
            .map_or_else(|| Value::String(value.to_string()), Value::Number);
        self.values.push((field.name(), value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.values.push((field.name(), Value::from(value)));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        // The error and its causes, as `{:#}` prints an anyhow chain.
        let mut text = value.to_string();
        let mut source = value.source();
        while let Some(cause) = source {
            let _ = write!(text, ": {cause}");
            source = cause.source();
        }
        self.values.push((field.name(), Value::String(text)));
    }
}

/// `2026-10-14T15:01:02.345678Z`.
fn rfc3339_micros(time: SystemTime) -> String {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:06}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        since.subsec_micros()
    )
}

/// The Gregorian date `days` after 1970-01-01 (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

/// How often a throttled call site may log.
pub const THROTTLE_INTERVAL: Duration = Duration::from_secs(10);

/// Limits one call site to a line per [`THROTTLE_INTERVAL`], for failures that can repeat on every
/// request. Use it through [`warn_throttled!`](crate::warn_throttled) or
/// [`error_throttled!`](crate::error_throttled).
#[derive(Debug)]
pub struct Throttle {
    /// Milliseconds since [`epoch`] of the last logged line, plus one; zero before the first.
    last: AtomicU64,
    suppressed: AtomicU64,
}

impl Throttle {
    /// A throttle that lets the first line through.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last: AtomicU64::new(0),
            suppressed: AtomicU64::new(0),
        }
    }

    /// `Some(n)` when this occurrence should be logged, `n` being how many were held back since
    /// the last line; `None` when it should not.
    pub fn check(&self) -> Option<u64> {
        self.check_at(epoch().elapsed())
    }

    fn check_at(&self, now: Duration) -> Option<u64> {
        let now = u64::try_from(now.as_millis())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let interval = u64::try_from(THROTTLE_INTERVAL.as_millis()).unwrap_or(u64::MAX);
        let last = self.last.load(Ordering::Relaxed);
        let due = last == 0 || now.saturating_sub(last) >= interval;
        if due
            && self
                .last
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            return Some(self.suppressed.swap(0, Ordering::Relaxed));
        }
        self.suppressed.fetch_add(1, Ordering::Relaxed);
        None
    }
}

impl Default for Throttle {
    fn default() -> Self {
        Self::new()
    }
}

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// `tracing::warn!`, at most once per [`THROTTLE_INTERVAL`] per call site; the occurrences in
/// between go to `debug`. A line after a quiet spell carries `suppressed`, the count held back.
#[macro_export]
macro_rules! warn_throttled {
    ($($arg:tt)+) => {{
        static THROTTLE: $crate::logging::Throttle = $crate::logging::Throttle::new();
        match THROTTLE.check() {
            Some(0) => ::tracing::warn!($($arg)+),
            Some(suppressed) => ::tracing::warn!(suppressed, $($arg)+),
            None => ::tracing::debug!($($arg)+),
        }
    }};
}

/// Like [`warn_throttled!`], at `error`.
#[macro_export]
macro_rules! error_throttled {
    ($($arg:tt)+) => {{
        static THROTTLE: $crate::logging::Throttle = $crate::logging::Throttle::new();
        match THROTTLE.check() {
            Some(0) => ::tracing::error!($($arg)+),
            Some(suppressed) => ::tracing::error!(suppressed, $($arg)+),
            None => ::tracing::debug!($($arg)+),
        }
    }};
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::{Arc, Mutex};

    use super::*;

    /// Captures every line `f` logs, through [`JsonFormat`].
    fn capture(f: impl FnOnce()) -> Vec<serde_json::Map<String, Value>> {
        capture_raw(f)
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn capture_raw(f: impl FnOnce()) -> Vec<String> {
        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(Level::TRACE)
            .event_format(JsonFormat)
            .with_writer(move || Sink(sink.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let text = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
        text.lines().map(str::to_owned).collect()
    }

    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn writes_the_envelope_then_the_fields() {
        let log = || {
            tracing::warn!(
                key = "gradle/abc",
                size = 42_u64,
                delta = -3_i64,
                ratio = 0.5,
                up = false,
                tier = %"disk",
                "bucket failed"
            );
        };
        let raw = &capture_raw(log)[0];
        let order: Vec<usize> = [
            "timestamp",
            "severity",
            "target",
            "message",
            "key",
            "size",
            "up",
        ]
        .iter()
        .map(|key| raw.find(&format!("\"{key}\":")).unwrap())
        .collect();
        assert!(order.is_sorted(), "{raw}");
        assert!(raw.starts_with("{\"timestamp\":"), "{raw}");
        let line = &capture(log)[0];
        assert_eq!(line["severity"], "WARNING");
        assert_eq!(line["target"], module_path!());
        assert_eq!(line["message"], "bucket failed");
        assert_eq!(line["key"], "gradle/abc");
        assert_eq!(line["size"], 42);
        assert_eq!(line["delta"], -3);
        assert_eq!(line["ratio"], 0.5);
        assert_eq!(line["up"], false);
        assert_eq!(line["tier"], "disk");
    }

    #[test]
    fn maps_every_level_to_a_cloud_logging_severity() {
        let lines = capture(|| {
            tracing::error!("e");
            tracing::warn!("w");
            tracing::info!("i");
            tracing::debug!("d");
            tracing::trace!("t");
        });
        let severities: Vec<&str> = lines
            .iter()
            .map(|l| l["severity"].as_str().unwrap())
            .collect();
        assert_eq!(severities, ["ERROR", "WARNING", "INFO", "DEBUG", "DEBUG"]);
    }

    #[test]
    fn escapes_what_a_client_sends() {
        let agent = "Gradle/8\n{\"severity\":\"ERROR\"}\u{1b}[31m\u{2028}";
        let lines = capture(|| tracing::info!(agent, "request"));
        assert_eq!(lines.len(), 1, "one event must stay one line");
        assert_eq!(lines[0]["agent"], agent);
        assert_eq!(lines[0]["severity"], "INFO");
    }

    #[test]
    fn renames_fields_that_clash_with_the_envelope() {
        let lines = capture(|| tracing::info!(severity = "fake", target = "x", "m"));
        assert_eq!(lines[0]["severity"], "INFO");
        assert_eq!(lines[0]["fields.severity"], "fake");
        assert_eq!(lines[0]["fields.target"], "x");
    }

    #[test]
    fn writes_an_error_with_its_causes() {
        let error = io::Error::other(io::Error::new(io::ErrorKind::NotFound, "no such bucket"));
        let lines = capture(|| {
            let error: &(dyn std::error::Error + 'static) = &error;
            tracing::warn!(error, "upload failed");
        });
        let text = lines[0]["error"].as_str().unwrap();
        assert!(text.contains("no such bucket"), "{text}");
    }

    #[test]
    fn formats_timestamps_in_utc_with_microseconds() {
        let at = |secs, micros| {
            rfc3339_micros(UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_micros(micros))
        };
        assert_eq!(at(0, 0), "1970-01-01T00:00:00.000000Z");
        assert_eq!(at(951_782_400, 7), "2000-02-29T00:00:00.000007Z");
        assert_eq!(at(1_791_990_062, 345_678), "2026-10-14T15:01:02.345678Z");
        assert_eq!(at(4_102_444_799, 999_999), "2099-12-31T23:59:59.999999Z");
    }

    #[test]
    fn throttle_lets_one_line_through_per_interval() {
        let throttle = Throttle::new();
        let ms = Duration::from_millis;
        assert_eq!(throttle.check_at(ms(5)), Some(0));
        assert_eq!(throttle.check_at(ms(6)), None);
        assert_eq!(throttle.check_at(ms(9_000)), None);
        assert_eq!(throttle.check_at(ms(10_005)), Some(2));
        assert_eq!(throttle.check_at(ms(10_006)), None);
        assert_eq!(throttle.check_at(ms(30_000)), Some(1));
    }

    #[test]
    fn throttled_macro_falls_back_to_debug() {
        let lines = capture(|| {
            for attempt in 0..3_u64 {
                crate::warn_throttled!(attempt, "tier read failed");
            }
        });
        let severities: Vec<&str> = lines
            .iter()
            .map(|l| l["severity"].as_str().unwrap())
            .collect();
        assert_eq!(severities, ["WARNING", "DEBUG", "DEBUG"]);
    }
}
