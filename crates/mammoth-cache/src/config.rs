//! The configuration file, `mammoth.toml` (design doc 0005).
//!
//! One file is the whole configuration. It is read once at startup and validated before anything
//! starts: unknown keys, wrong types, unparseable units and contradictions are errors.

use std::{
    collections::BTreeMap,
    env, fmt, fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use serde::{Deserialize, Deserializer, de};

pub use crate::auth::Scope;
use crate::{
    auth::{Authenticator, Scopes},
    store::memory::EvictionPolicy,
};

/// The only schema version this build reads.
pub const VERSION: u32 = 1;

/// Where the config file is looked for when neither `--config` nor `MAMMOTH_CONFIG` is given.
pub const DEFAULT_PATH: &str = "mammoth.toml";

/// The environment variable naming the config file.
pub const PATH_ENV: &str = "MAMMOTH_CONFIG";

/// A parsed and validated `mammoth.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The file's schema version. Must be [`VERSION`].
    pub version: u32,
    /// The address the server listens on.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// Connection timeouts and shutdown (design doc 0013).
    #[serde(default)]
    pub server: Server,
    /// TLS settings; absent to serve plain HTTP.
    pub tls: Option<Tls>,
    /// Which client protocols are served.
    #[serde(default)]
    pub protocols: Protocols,
    /// Limits that apply to every tier.
    #[serde(default)]
    pub limits: Limits,
    /// L1.
    #[serde(default)]
    pub memory: Memory,
    /// L2. Without it the node is memory-only, which suits tests and trials.
    pub disk: Option<Disk>,
    /// L3, optional.
    pub bucket: Option<Bucket>,
    /// Users, tokens and scopes (design doc 0002).
    #[serde(default)]
    pub auth: Auth,
    /// The `/metrics` endpoint.
    #[serde(default)]
    pub metrics: Metrics,
    /// Logging.
    #[serde(default)]
    pub log: Log,
}

/// `[server]`: how connections are timed out and how the node shuts down.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Server {
    /// After a shutdown signal, how long to keep serving while load balancers notice that
    /// `/readyz` fails.
    pub shutdown_delay: HumanDuration,
    /// From the shutdown signal, how long to wait for requests and bucket uploads to finish.
    pub shutdown_timeout: HumanDuration,
    /// How long a connection may wait for, and take to send, its next request's headers. Must
    /// exceed the load balancer's idle timeout.
    pub idle_timeout: HumanDuration,
    /// How long an upload may send nothing before it is abandoned. Per read, not in total.
    pub body_read_timeout: HumanDuration,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            shutdown_delay: HumanDuration(Duration::from_secs(5)),
            shutdown_timeout: HumanDuration(Duration::from_secs(25)),
            idle_timeout: HumanDuration(Duration::from_secs(75)),
            body_read_timeout: HumanDuration(Duration::from_secs(60)),
        }
    }
}

/// `[tls]`: serve HTTPS with this certificate (design doc 0014). The files are read and checked
/// at startup, and re-read while serving, so a renewed certificate is picked up without a restart.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tls {
    /// PEM certificate chain.
    pub cert: PathBuf,
    /// PEM private key.
    pub key: PathBuf,
}

/// `[protocols]`: which client protocols the node serves. A protocol that is off has no routes,
/// so its requests get the same `404` as any unknown path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Protocols {
    /// Gradle's HTTP build cache, `/cache/{key}`.
    pub gradle: bool,
    /// The Apache Maven Build Cache Extension, `/maven/{path}`.
    pub maven: bool,
    /// Bazel's HTTP remote cache, `/bazel/ac/{digest}` and `/bazel/cas/{digest}`. Off unless asked
    /// for.
    pub bazel: bool,
}

impl Default for Protocols {
    fn default() -> Self {
        Self {
            gradle: true,
            maven: true,
            bazel: false,
        }
    }
}

/// `[limits]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    /// The largest entry accepted. The Build Cache Node's `cache.maxArtifactSize`.
    pub max_entry_size: ByteSize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_entry_size: ByteSize::mib(100),
        }
    }
}

/// `[memory]`, the L1 tier.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Memory {
    /// Bytes the tier may hold.
    pub size: ByteSize,
    /// Larger entries skip memory.
    pub max_entry_size: ByteSize,
    /// The eviction policy (design doc 0010).
    #[serde(deserialize_with = "from_str")]
    pub eviction: EvictionPolicy,
}

impl Default for Memory {
    fn default() -> Self {
        Self {
            size: ByteSize::gib(1),
            max_entry_size: ByteSize::mib(1),
            eviction: EvictionPolicy::default(),
        }
    }
}

/// `[disk]`, the L2 tier.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Disk {
    /// The directory the tier owns.
    pub path: PathBuf,
    /// Bytes the tier may use. The Build Cache Node's `cache.targetSize`.
    #[serde(default = "default_disk_size")]
    pub size: ByteSize,
    /// Free disk space to keep. The Build Cache Node's `cache.freeSpaceBufferSize`.
    #[serde(default = "default_free_space_buffer")]
    pub free_space_buffer: ByteSize,
    /// Entries older than this are evicted. The Build Cache Node's `cache.maxEntryAgeInHours`.
    pub max_entry_age: Option<HumanDuration>,
}

/// `[bucket]`, the L3 tier.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bucket {
    /// `s3://`, `gs://`, `az://` or `file://`, with an optional key prefix.
    pub url: String,
    /// The bucket's region (S3).
    pub region: Option<String>,
    /// An S3-compatible endpoint such as `MinIO` or Cloudflare R2.
    pub endpoint: Option<String>,
    /// Allows an `http://` endpoint, such as a local `MinIO`, `SeaweedFS` or Azurite.
    #[serde(default)]
    pub allow_http: bool,
    /// When a `PUT` is answered relative to the upload.
    #[serde(default)]
    pub write: BucketWrite,
    /// Uploads waiting in the background, for [`BucketWrite::Async`].
    #[serde(default = "default_upload_queue")]
    pub upload_queue: usize,
}

/// `bucket.write`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BucketWrite {
    /// Answer once the entry is on disk; upload in the background.
    #[default]
    Async,
    /// Answer only after the upload.
    Sync,
}

/// `[auth]` (design doc 0002).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Auth {
    /// Scopes a request without credentials gets. The Build Cache Node's `anonymousLevel`.
    pub anonymous: Vec<Scope>,
    /// Users by name.
    pub users: BTreeMap<String, User>,
}

impl Auth {
    /// Whether the file configures any access control. Without users, an anonymous scope list
    /// or `metrics.require_auth`, the node is open to everyone, as a fresh install should be.
    #[must_use]
    pub fn is_configured(&self, metrics: &Metrics) -> bool {
        !self.users.is_empty() || !self.anonymous.is_empty() || metrics.require_auth
    }

    /// The authenticator these settings describe.
    #[must_use]
    pub fn authenticator(&self, metrics: &Metrics) -> Authenticator {
        if !self.is_configured(metrics) {
            return Authenticator::open();
        }
        Authenticator::new(
            self.anonymous.iter().copied().collect::<Scopes>(),
            self.users.iter().map(|(name, user)| {
                (
                    name.clone(),
                    user.token_hashes.iter().map(|h| h.0).collect(),
                    user.scopes.iter().copied().collect(),
                )
            }),
            metrics.require_auth,
        )
    }
}

/// `[auth.users.<name>]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct User {
    /// Free text, for people reading the file.
    pub note: Option<String>,
    /// What the user may do.
    pub scopes: Vec<Scope>,
    /// Hashes of the user's tokens; any one of them authenticates.
    pub token_hashes: Vec<TokenHash>,
}

/// A token's SHA-256 hash, written `sha256:<64 hex characters>`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenHash(pub [u8; 32]);

impl fmt::Debug for TokenHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenHash(sha256:")?;
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        f.write_str(")")
    }
}

impl FromStr for TokenHash {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid =
            || format!("token hash {s:?} must be \"sha256:\" and 64 lowercase hex digits");
        let hex = s.strip_prefix("sha256:").ok_or_else(invalid)?;
        if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(invalid());
        }
        let mut hash = [0_u8; 32];
        for (i, byte) in hash.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).map_err(|_| invalid())?;
        }
        Ok(Self(hash))
    }
}

impl<'de> Deserialize<'de> for TokenHash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

/// `[metrics]`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Metrics {
    /// Whether `/metrics` needs the `metrics:read` scope.
    pub require_auth: bool,
}

/// `[log]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Log {
    /// The most verbose level logged.
    pub level: LogLevel,
    /// The line format.
    pub format: LogFormat,
    /// Whether every request is logged.
    pub access_log: bool,
}

impl Default for Log {
    fn default() -> Self {
        Self {
            level: LogLevel::Info,
            format: LogFormat::Json,
            access_log: true,
        }
    }
}

/// `log.level`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[allow(missing_docs)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    /// The level's name, as a `tracing` filter directive.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

/// `log.format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// One JSON object per line.
    Json,
    /// Human-readable lines.
    Text,
}

/// Deserializes a string through `FromStr`.
fn from_str<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    T::Err: fmt::Display,
{
    String::deserialize(deserializer)?
        .parse()
        .map_err(de::Error::custom)
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 5071))
}

fn default_disk_size() -> ByteSize {
    ByteSize::gib(10)
}

fn default_free_space_buffer() -> ByteSize {
    ByteSize::gib(1)
}

fn default_upload_queue() -> usize {
    1000
}

/// A config file that could not be used.
#[derive(Debug)]
pub enum ConfigError {
    /// The file could not be read.
    Read(PathBuf, std::io::Error),
    /// The file is not valid TOML or does not match the schema.
    Parse(PathBuf, String),
    /// The file parsed but its settings contradict each other or are unsupported.
    Invalid(PathBuf, String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(path, e) => write!(f, "cannot read {}: {e}", path.display()),
            Self::Parse(path, e) | Self::Invalid(path, e) => write!(f, "{}: {e}", path.display()),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// The config file's path: `explicit` (from `--config`), else [`PATH_ENV`], else
    /// [`DEFAULT_PATH`].
    #[must_use]
    pub fn locate(explicit: Option<PathBuf>) -> PathBuf {
        explicit
            .or_else(|| env::var_os(PATH_ENV).map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(DEFAULT_PATH))
    }

    /// Reads, parses and validates the file at `path`.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(|e| ConfigError::Read(path.to_owned(), e))?;
        Self::parse(&text).map_err(|e| match e {
            ParseError::Syntax(msg) => ConfigError::Parse(path.to_owned(), msg),
            ParseError::Invalid(msg) => ConfigError::Invalid(path.to_owned(), msg),
        })
    }

    /// The settings used when no config file is given and `./mammoth.toml` does not exist: every
    /// default, and no disk or bucket.
    #[must_use]
    pub fn fallback() -> Self {
        match Self::parse("version = 1") {
            Ok(config) => config,
            Err(_) => unreachable!("the defaults are valid"),
        }
    }

    /// Parses and validates a config file's contents.
    fn parse(text: &str) -> Result<Self, ParseError> {
        let config: Self = toml::from_str(text).map_err(|e| ParseError::Syntax(e.to_string()))?;
        config.validate().map_err(ParseError::Invalid)?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        if self.version != VERSION {
            return Err(format!(
                "version {} is not supported; this build reads version {VERSION}",
                self.version
            ));
        }
        if !self.protocols.gradle && !self.protocols.maven && !self.protocols.bazel {
            return Err(
                "every protocol in [protocols] is off; the node would serve nothing".to_owned(),
            );
        }
        if self.memory.max_entry_size > self.limits.max_entry_size {
            return Err(format!(
                "memory.max_entry_size ({}) is above limits.max_entry_size ({})",
                self.memory.max_entry_size, self.limits.max_entry_size
            ));
        }
        if let Some(disk) = &self.disk
            && self.limits.max_entry_size > disk.size
        {
            return Err(format!(
                "limits.max_entry_size ({}) is above disk.size ({})",
                self.limits.max_entry_size, disk.size
            ));
        }
        if let Some(bucket) = &self.bucket
            && bucket.write == BucketWrite::Async
            && bucket.upload_queue == 0
        {
            return Err("bucket.upload_queue must be at least 1".to_owned());
        }
        let server = &self.server;
        for (name, value) in [
            ("shutdown_timeout", server.shutdown_timeout),
            ("idle_timeout", server.idle_timeout),
            ("body_read_timeout", server.body_read_timeout),
        ] {
            if value.0.is_zero() {
                return Err(format!("server.{name} must be above zero"));
            }
        }
        if server.shutdown_delay >= server.shutdown_timeout {
            return Err(format!(
                "server.shutdown_delay ({}) must be below server.shutdown_timeout ({})",
                server.shutdown_delay, server.shutdown_timeout
            ));
        }
        for (name, user) in &self.auth.users {
            if user.token_hashes.is_empty() {
                return Err(format!("auth.users.{name} has no token_hashes"));
            }
        }
        Ok(())
    }
}

enum ParseError {
    Syntax(String),
    Invalid(String),
}

/// A size in bytes, written as an integer or with a binary unit: `512`, `64KiB`, `100MiB`, `1GiB`,
/// `2TiB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ByteSize(pub u64);

impl ByteSize {
    /// `n` mebibytes.
    #[must_use]
    pub const fn mib(n: u64) -> Self {
        Self(n << 20)
    }

    /// `n` gibibytes.
    #[must_use]
    pub const fn gib(n: u64) -> Self {
        Self(n << 30)
    }

    /// The size in bytes.
    #[must_use]
    pub const fn bytes(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (shift, unit) in [(40, "TiB"), (30, "GiB"), (20, "MiB"), (10, "KiB")] {
            if self.0 >= 1 << shift && self.0.is_multiple_of(1 << shift) {
                return write!(f, "{}{unit}", self.0 >> shift);
            }
        }
        write!(f, "{}B", self.0)
    }
}

impl FromStr for ByteSize {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid =
            || format!("{s:?} is not a size; use a whole number with B, KiB, MiB, GiB or TiB");
        let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        let (number, unit) = s.split_at(split);
        let number: u64 = number.parse().map_err(|_| invalid())?;
        let shift = match unit.trim_start() {
            "" | "B" => 0,
            "KiB" => 10,
            "MiB" => 20,
            "GiB" => 30,
            "TiB" => 40,
            _ => return Err(invalid()),
        };
        number
            .checked_mul(1 << shift)
            .map(Self)
            .ok_or_else(|| format!("{s:?} is too large"))
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bytes(u64),
            Text(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Bytes(n) => Ok(Self(n)),
            Raw::Text(s) => s.parse().map_err(de::Error::custom),
        }
    }
}

/// A duration written as a whole number with a unit: `30s`, `15m`, `12h`, `30d`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct HumanDuration(pub Duration);

impl FromStr for HumanDuration {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || format!("{s:?} is not a duration; use a whole number with s, m, h or d");
        let split = s.find(|c: char| !c.is_ascii_digit()).ok_or_else(invalid)?;
        let (number, unit) = s.split_at(split);
        let number: u64 = number.parse().map_err(|_| invalid())?;
        let seconds = match unit.trim_start() {
            "s" => 1,
            "m" => 60,
            "h" => 60 * 60,
            "d" => 24 * 60 * 60,
            _ => return Err(invalid()),
        };
        number
            .checked_mul(seconds)
            .map(|s| Self(Duration::from_secs(s)))
            .ok_or_else(|| format!("{s:?} is too long"))
    }
}

impl fmt::Display for HumanDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.0.as_secs();
        for (unit, size) in [("d", 24 * 60 * 60), ("h", 60 * 60), ("m", 60)] {
            if secs != 0 && secs.is_multiple_of(size) {
                return write!(f, "{}{unit}", secs / size);
            }
        }
        write!(f, "{secs}s")
    }
}

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "version = 1\n[disk]\npath = \"/var/lib/mammoth\"\n";

    fn parse(text: &str) -> Result<Config, String> {
        Config::parse(text).map_err(|e| match e {
            ParseError::Syntax(m) | ParseError::Invalid(m) => m,
        })
    }

    #[test]
    fn minimal_file_gets_the_defaults() {
        let config = parse(MINIMAL).unwrap();
        assert_eq!(config.listen, default_listen());
        assert_eq!(config.limits.max_entry_size, ByteSize::mib(100));
        assert_eq!(config.memory.size, ByteSize::gib(1));
        assert_eq!(config.memory.eviction, EvictionPolicy::S3Fifo);
        let disk = config.disk.unwrap();
        assert_eq!(disk.size, ByteSize::gib(10));
        assert_eq!(disk.free_space_buffer, ByteSize::gib(1));
        assert!(config.bucket.is_none());
        assert!(config.auth.anonymous.is_empty() && config.auth.users.is_empty());
        assert_eq!(config.log.format, LogFormat::Json);
        assert_eq!(config.protocols, Protocols::default());
        assert!(config.protocols.gradle && config.protocols.maven && !config.protocols.bazel);
    }

    #[test]
    fn protocols_switch_on_and_off() {
        let config = parse(&format!(
            "{MINIMAL}[protocols]\nmaven = false\nbazel = true\n"
        ))
        .unwrap();
        assert_eq!(
            config.protocols,
            Protocols {
                gradle: true,
                maven: false,
                bazel: true,
            }
        );
    }

    #[test]
    fn design_doc_example_parses() {
        let text = r#"
            version = 1
            listen = "0.0.0.0:5071"

            [limits]
            max_entry_size = "100MiB"

            [memory]
            size = "1GiB"
            max_entry_size = "1MiB"
            eviction = "w-tinylfu"

            [disk]
            path = "/var/lib/mammoth"
            size = "50GiB"
            free_space_buffer = "1GiB"
            max_entry_age = "30d"

            [bucket]
            url = "s3://acme-build-cache/mammoth/"
            region = "us-east-1"
            allow_http = true
            write = "async"
            upload_queue = 1000

            [auth]
            anonymous = []

            [auth.users.ci]
            note = "Continuous integration"
            scopes = ["cache:read", "cache:write"]
            token_hashes = ["sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"]

            [metrics]
            require_auth = false

            [log]
            level = "info"
            format = "text"
            access_log = true
        "#;
        let config = parse(text).unwrap();
        assert_eq!(config.memory.eviction, EvictionPolicy::WTinyLfu);
        let disk = config.disk.unwrap();
        assert_eq!(disk.size, ByteSize::gib(50));
        let age = disk.max_entry_age.unwrap().0;
        assert_eq!(age, Duration::from_hours(30 * 24));
        let bucket = config.bucket.unwrap();
        assert_eq!(bucket.region.as_deref(), Some("us-east-1"));
        assert!(bucket.allow_http);
        let ci = &config.auth.users["ci"];
        assert_eq!(ci.scopes, [Scope::CacheRead, Scope::CacheWrite]);
        assert_eq!(ci.token_hashes[0].0[0], 0x9f);
        assert_eq!(config.log.format, LogFormat::Text);
    }

    #[test]
    fn server_settings_have_defaults_and_are_checked() {
        let server = parse(MINIMAL).unwrap().server;
        assert_eq!(server.shutdown_delay.0, Duration::from_secs(5));
        assert_eq!(server.shutdown_timeout.0, Duration::from_secs(25));
        assert_eq!(server.idle_timeout.0, Duration::from_secs(75));
        assert_eq!(server.body_read_timeout.0, Duration::from_mins(1));

        let custom = parse("version = 1\n[server]\nidle_timeout = \"11m\"\n").unwrap();
        assert_eq!(custom.server.idle_timeout.0, Duration::from_mins(11));
        let cases = [
            (
                "version = 1\n[server]\nshutdown_delay = \"30s\"\n",
                "server.shutdown_delay (30s) must be below server.shutdown_timeout (25s)",
            ),
            (
                "version = 1\n[server]\nidle_timeout = \"0s\"\n",
                "server.idle_timeout must be above zero",
            ),
            (
                "version = 1\n[server]\nheader_read_timeout = \"30s\"\n",
                "unknown field `header_read_timeout`",
            ),
        ];
        for (text, expected) in cases {
            let error = parse(text).unwrap_err();
            assert!(error.contains(expected), "{error:?} lacks {expected:?}");
        }
    }

    #[test]
    fn errors_name_the_problem() {
        let cases = [
            ("[disk]\npath = \"/d\"\n", "missing field `version`"),
            (
                "version = 1\n[disk]\nsize = \"1GiB\"\n",
                "missing field `path`",
            ),
            (
                "version = 2\n[disk]\npath = \"/d\"\n",
                "version 2 is not supported",
            ),
            (
                &format!("{MINIMAL}colour = \"orange\"\n"),
                "unknown field `colour`",
            ),
            (
                &format!("{MINIMAL}size = \"lots\"\n"),
                "\"lots\" is not a size",
            ),
            (
                "version = 1\n[disk]\npath = \"/d\"\nsize = \"10GB\"\n",
                "\"10GB\" is not a size",
            ),
            (
                "version = 1\n[disk]\npath = \"/d\"\nmax_entry_age = \"1w\"\n",
                "\"1w\" is not a duration",
            ),
            (
                "version = 1\n[memory]\nmax_entry_size = \"200MiB\"\n[disk]\npath = \"/d\"\n",
                "memory.max_entry_size (200MiB) is above limits.max_entry_size (100MiB)",
            ),
            (
                "version = 1\n[memory]\neviction = \"random\"\n[disk]\npath = \"/d\"\n",
                "unknown eviction policy \"random\"",
            ),
            (
                &format!("{MINIMAL}[auth]\nanonymous = [\"cache:delete\"]\n"),
                "unknown variant `cache:delete`",
            ),
            (
                &format!("{MINIMAL}[auth.users.ci]\nscopes = []\ntoken_hashes = [\"abc\"]\n"),
                "must be \"sha256:\" and 64",
            ),
            (
                &format!("{MINIMAL}[tls]\ncert = \"c\"\n"),
                "missing field `key`",
            ),
            (
                &format!("{MINIMAL}[tls]\ncert = \"c\"\nkey = \"k\"\nclient_ca = \"ca\"\n"),
                "unknown field `client_ca`",
            ),
            (
                &format!("{MINIMAL}[protocols]\ngradle = false\nmaven = false\n"),
                "every protocol in [protocols] is off",
            ),
            (
                &format!("{MINIMAL}[protocols]\nreapi = true\n"),
                "unknown field `reapi`",
            ),
        ];
        for (text, expected) in cases {
            let err = parse(text).unwrap_err();
            assert!(err.contains(expected), "{text:?}: {err}");
        }
    }

    #[test]
    fn auth_is_open_until_configured() {
        let open = parse(MINIMAL).unwrap();
        assert!(open.auth.authenticator(&open.metrics).is_open());

        let text = format!(
            "{MINIMAL}[auth]\nanonymous = [\"cache:read\"]\n[auth.users.ci]\n\
             scopes = [\"cache:read\", \"cache:write\"]\ntoken_hashes = [\"sha256:{}\"]\n",
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        );
        let config = parse(&text).unwrap();
        let auth = config.auth.authenticator(&config.metrics);
        assert!(!auth.is_open());
        let anonymous = auth.authenticate(None).unwrap();
        assert!(anonymous.scopes.contains(Scope::CacheRead));
        assert!(!anonymous.scopes.contains(Scope::CacheWrite));
        let ci = auth
            .authenticate(Some(crate::auth::Credentials::Basic {
                user: "ci",
                token: "test",
            }))
            .unwrap();
        assert!(ci.scopes.contains(Scope::CacheWrite));
    }

    #[test]
    fn fallback_is_memory_only() {
        let config = Config::fallback();
        assert!(config.disk.is_none() && config.bucket.is_none());
    }

    #[test]
    fn sizes_and_durations() {
        assert_eq!("512".parse(), Ok(ByteSize(512)));
        assert_eq!("64KiB".parse(), Ok(ByteSize(64 << 10)));
        assert_eq!("2 TiB".parse(), Ok(ByteSize(2 << 40)));
        assert!("1.5GiB".parse::<ByteSize>().is_err());
        assert!("99999999TiB".parse::<ByteSize>().is_err());
        assert_eq!(ByteSize::mib(100).to_string(), "100MiB");
        assert_eq!(ByteSize(1000).to_string(), "1000B");
        assert_eq!("90m".parse(), Ok(HumanDuration(Duration::from_mins(90))));
        assert!("30".parse::<HumanDuration>().is_err());
    }
}
