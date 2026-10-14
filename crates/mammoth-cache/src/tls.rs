//! Optional built-in TLS (design doc 0014): the serving certificate, the checks it must pass, and
//! picking up a renewed one from disk.
//!
//! [`Certificates`] holds the current certificate and key and answers rustls's request for them
//! on every handshake. [`Certificates::watch`] re-reads the files on an interval and swaps in a
//! new pair only when it passes the same checks as at startup, so a bad renewal keeps the old
//! certificate serving.

use std::{
    fmt, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio_rustls::rustls::{
    self, ServerConfig,
    crypto::{CryptoProvider, aws_lc_rs},
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};

/// How long a client may take to complete the TLS handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the certificate and key files are re-read.
pub const RELOAD_INTERVAL: Duration = Duration::from_secs(60);

/// A certificate that expires sooner than this is logged as a warning.
const EXPIRY_WARNING: Duration = Duration::from_hours(14 * 24);

/// The serving certificate and key, loaded from PEM files and reloaded when they change.
pub struct Certificates {
    cert_path: PathBuf,
    key_path: PathBuf,
    current: RwLock<Arc<CertifiedKey>>,
    /// A hash of the file contents behind `current`, to skip parsing when nothing changed. Only
    /// the hash, so the key's PEM is not kept in memory.
    loaded: Mutex<blake3::Hash>,
}

impl fmt::Debug for Certificates {
    // Never print the key.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Certificates")
            .field("cert_path", &self.cert_path)
            .field("key_path", &self.key_path)
            .finish_non_exhaustive()
    }
}

impl Certificates {
    /// Reads and checks the certificate chain and private key. Fails if a file can't be read,
    /// holds no certificate or key, or the key doesn't match the certificate.
    pub fn load(cert_path: impl Into<PathBuf>, key_path: impl Into<PathBuf>) -> io::Result<Self> {
        let cert_path = cert_path.into();
        let key_path = key_path.into();
        let (cert_pem, key_pem) = read_pair(&cert_path, &key_path)?;
        let key = parse(&cert_pem, &key_pem, &cert_path, &key_path)?;
        log_loaded(&key, "TLS certificate loaded");
        warn_if_world_readable(&key_path);
        Ok(Self {
            cert_path,
            key_path,
            current: RwLock::new(Arc::new(key)),
            loaded: Mutex::new(pair_hash(&cert_pem, &key_pem)),
        })
    }

    /// Re-reads the files and swaps in the new certificate if they changed and pass the checks.
    /// Returns whether it swapped. On an error the current certificate stays.
    pub fn reload(&self) -> io::Result<bool> {
        let (cert_pem, key_pem) = read_pair(&self.cert_path, &self.key_path)?;
        let hash = pair_hash(&cert_pem, &key_pem);
        let mut loaded = self.loaded.lock().unwrap_or_else(PoisonError::into_inner);
        if *loaded == hash {
            return Ok(false);
        }
        let key = Arc::new(parse(&cert_pem, &key_pem, &self.cert_path, &self.key_path)?);
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = key;
        *loaded = hash;
        warn_if_world_readable(&self.key_path);
        Ok(true)
    }

    /// The certificate new handshakes get.
    pub fn current(&self) -> Arc<CertifiedKey> {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// A rustls server config that serves the current certificate: TLS 1.2 and 1.3 with rustls's
    /// default suites, no client certificates, and ALPN `http/1.1` only.
    pub fn server_config(self: &Arc<Self>) -> io::Result<Arc<ServerConfig>> {
        let mut config = ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
            .map_err(io::Error::other)?
            .with_no_client_auth()
            .with_cert_resolver(self.clone());
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Arc::new(config))
    }

    /// Re-reads the files every `interval`, forever. Publishes the expiry gauge at the start and
    /// after each reload.
    pub async fn watch(self: Arc<Self>, interval: Duration) {
        publish_expiry(&self.current());
        let mut ticks = tokio::time::interval(interval);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticks.tick().await; // The first tick is immediate; the files were just loaded.
        loop {
            ticks.tick().await;
            // File reads block, and a stalled mount (a Secret volume, NFS) must not stall a
            // runtime worker.
            let certs = self.clone();
            let Ok(reloaded) = tokio::task::spawn_blocking(move || certs.reload()).await else {
                continue;
            };
            match reloaded {
                Ok(false) => {}
                Ok(true) => {
                    metrics::counter!("mammoth_tls_reloads_total", "result" => "ok").increment(1);
                    let current = self.current();
                    log_loaded(&current, "TLS certificate reloaded");
                    publish_expiry(&current);
                }
                Err(error) => {
                    metrics::counter!("mammoth_tls_reloads_total", "result" => "error")
                        .increment(1);
                    tracing::warn!(%error, "TLS certificate not reloaded; still serving the old one");
                }
            }
        }
    }
}

impl ResolvesServerCert for Certificates {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.current())
    }
}

/// How long a generated certificate is valid: 397 days, the most browsers and Apple clients accept.
pub const GENERATED_VALIDITY: Duration = Duration::from_hours(397 * 24);

/// The common name of a generated certificate.
const GENERATED_NAME: &str = "mammoth-cache";

/// For `[tls] generate`: writes a self-signed certificate and key to the two paths when neither
/// exists, and leaves both alone when both do. Returns whether it wrote them.
///
/// The certificate is ECDSA P-256, valid for [`GENERATED_VALIDITY`], for `hostnames` (or, when
/// empty, the machine's hostname, `localhost`, `127.0.0.1` and `::1`). Each file is written to a
/// temporary file and renamed, the key first and readable by the owner only, so a crash never
/// leaves half a key.
pub fn generate_if_missing(
    cert_path: &Path,
    key_path: &Path,
    hostnames: &[String],
) -> io::Result<bool> {
    if !both_missing(cert_path, key_path)? {
        return Ok(false);
    }
    let hostnames = if hostnames.is_empty() {
        default_hostnames()
    } else {
        hostnames.to_vec()
    };
    let (cert_pem, key_pem) = self_signed(hostnames)
        .map_err(|error| io::Error::other(format!("cannot generate a TLS certificate: {error}")))?;
    write_new(key_path, key_pem.as_bytes(), 0o600)?;
    write_new(cert_path, cert_pem.as_bytes(), 0o644)?;
    Ok(true)
}

/// Whether `[tls] generate` would write a new pair: true when neither file exists, false when both
/// do, and an error when only one does, as that pair can't be served.
pub fn both_missing(cert_path: &Path, key_path: &Path) -> io::Result<bool> {
    let exists = |path: &Path| {
        path.try_exists().map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot check {}: {error}", path.display()),
            )
        })
    };
    match (exists(cert_path)?, exists(key_path)?) {
        (false, false) => Ok(true),
        (true, true) => Ok(false),
        (cert_exists, _) => {
            let (present, missing) = if cert_exists {
                (cert_path, key_path)
            } else {
                (key_path, cert_path)
            };
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "{} exists but {} does not; restore it, or delete both to generate a new \
                     self-signed pair",
                    present.display(),
                    missing.display()
                ),
            ))
        }
    }
}

/// The SHA-256 fingerprint of a certificate, as colon-separated hex (`AB:CD:...`), the form
/// `openssl x509 -fingerprint -sha256` and `keytool -printcert` print.
#[must_use]
pub fn fingerprint(cert: &CertificateDer<'_>) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(cert)
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// The machine's hostname, if it has one, then `localhost` and the loopback addresses.
fn default_hostnames() -> Vec<String> {
    let mut names = Vec::new();
    #[cfg(unix)]
    {
        let uname = rustix::system::uname();
        let host = uname.nodename().to_string_lossy();
        if !host.is_empty() && host != "localhost" {
            names.push(host.into_owned());
        }
    }
    names.extend(["localhost", "127.0.0.1", "::1"].map(String::from));
    names
}

/// A new self-signed certificate and its key, both PEM.
fn self_signed(hostnames: Vec<String>) -> Result<(String, String), rcgen::Error> {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
    // rcgen makes an IP address SAN of each name that parses as one.
    let mut params = rcgen::CertificateParams::new(hostnames)?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, GENERATED_NAME);
    // An hour back, so a client whose clock is a little behind still accepts it.
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::HOUR;
    params.not_after = now + GENERATED_VALIDITY;
    let cert = params.self_signed(&key)?;
    Ok((cert.pem(), key.serialize_pem()))
}

/// Writes a file that doesn't exist yet: to a temporary file beside it with `mode`, then renamed
/// into place, creating the directory if needed.
fn write_new(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    use std::io::Write as _;
    let context = |error: io::Error| {
        io::Error::new(
            error.kind(),
            format!("cannot write {}: {error}", path.display()),
        )
    };
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(context)?;
    }
    let mut temp_name = path.file_name().unwrap_or_default().to_owned();
    temp_name.push(".tmp");
    let temp = path.with_file_name(temp_name);
    // A leftover from a crash may have other permissions; start over.
    match std::fs::remove_file(&temp) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(context(error)),
        _ => {}
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, mode);
    #[cfg(not(unix))]
    let _ = mode;
    let mut file = options.open(&temp).map_err(context)?;
    file.write_all(contents).map_err(context)?;
    file.sync_all().map_err(context)?;
    drop(file);
    std::fs::rename(&temp, path).map_err(context)
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(aws_lc_rs::default_provider())
}

/// Hashes a certificate and key pair; the length prefix keeps the boundary between them.
fn pair_hash(cert_pem: &[u8], key_pem: &[u8]) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(cert_pem.len() as u64).to_le_bytes());
    hasher.update(cert_pem);
    hasher.update(key_pem);
    hasher.finalize()
}

/// Warns when any user on the machine may read the private key.
fn warn_if_world_readable(key_path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(key_path)
            && metadata.permissions().mode() & 0o004 != 0
        {
            tracing::warn!(
                path = %key_path.display(),
                "the TLS private key is readable by every user; make it readable by the node only \
                 (in Kubernetes, defaultMode 0440 on the Secret volume, with fsGroup)"
            );
        }
    }
    #[cfg(not(unix))]
    let _ = key_path;
}

fn read_pair(cert_path: &Path, key_path: &Path) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let read = |path: &Path, what: &str| {
        std::fs::read(path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot read the TLS {what} {}: {error}", path.display()),
            )
        })
    };
    Ok((read(cert_path, "certificate")?, read(key_path, "key")?))
}

fn parse(
    cert_pem: &[u8],
    key_pem: &[u8],
    cert_path: &Path,
    key_path: &Path,
) -> io::Result<CertifiedKey> {
    let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidData, message);
    let chain = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| invalid(format!("{}: {error}", cert_path.display())))?;
    if chain.is_empty() {
        return Err(invalid(format!(
            "{} holds no PEM CERTIFICATE block",
            cert_path.display()
        )));
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|error| {
        invalid(format!(
            "{} holds no unencrypted PEM private key (PKCS#8, PKCS#1 or SEC1): {error}",
            key_path.display()
        ))
    })?;
    CertifiedKey::from_der(chain, key, &provider()).map_err(|error| {
        let reason = match error {
            rustls::Error::InconsistentKeys(_) => "the key does not match the certificate".into(),
            other => other.to_string(),
        };
        invalid(format!(
            "{} and {}: {reason}",
            cert_path.display(),
            key_path.display()
        ))
    })
}

fn log_loaded(key: &CertifiedKey, message: &str) {
    let Some(expiry) = key.end_entity_cert().ok().and_then(|cert| not_after(cert)) else {
        tracing::info!(expires = "unknown", "{message}");
        return;
    };
    let expires = format_utc(expiry);
    tracing::info!(%expires, "{message}");
    let now = unix_now();
    if expiry <= now {
        tracing::warn!(%expires, "the TLS certificate has expired; clients will refuse it");
    } else if expiry - now < EXPIRY_WARNING.as_secs() {
        tracing::warn!(%expires, "the TLS certificate expires within 14 days");
    }
}

fn publish_expiry(key: &CertifiedKey) {
    if let Some(expiry) = key.end_entity_cert().ok().and_then(|cert| not_after(cert)) {
        #[allow(clippy::cast_precision_loss)] // Seconds until the year 285 million are exact.
        metrics::gauge!("mammoth_tls_certificate_expiry_seconds").set(expiry as f64);
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// The certificate's `notAfter`, in Unix seconds, or `None` if it can't be read.
///
/// Walks just enough DER to reach it: `Certificate ::= SEQUENCE { tbsCertificate SEQUENCE {
/// [0] version OPTIONAL, serialNumber, signature, issuer, validity SEQUENCE { notBefore, notAfter
/// }, ... }, ... }` (RFC 5280, 4.1). Used only for logs and a gauge, never to accept or refuse a
/// certificate.
fn not_after(cert: &[u8]) -> Option<u64> {
    let (_, certificate, _) = tlv(cert).filter(|(tag, _, _)| *tag == SEQUENCE)?;
    let (_, tbs, _) = tlv(certificate).filter(|(tag, _, _)| *tag == SEQUENCE)?;
    let (tag, _, mut rest) = tlv(tbs)?;
    if tag == VERSION {
        rest = tlv(rest)?.2; // Skip the serial number too.
    }
    rest = tlv(rest)?.2; // signature
    rest = tlv(rest)?.2; // issuer
    let (_, validity, _) = tlv(rest).filter(|(tag, _, _)| *tag == SEQUENCE)?;
    let not_before_rest = tlv(validity)?.2;
    let (tag, time, _) = tlv(not_before_rest)?;
    parse_time(tag, time)
}

/// Whether the certificate names itself as its issuer, as a self-signed certificate does.
#[must_use]
pub fn is_self_signed(cert: &[u8]) -> bool {
    let fields = || {
        let (_, certificate, _) = tlv(cert).filter(|(tag, _, _)| *tag == SEQUENCE)?;
        let (_, tbs, _) = tlv(certificate).filter(|(tag, _, _)| *tag == SEQUENCE)?;
        let (tag, _, mut rest) = tlv(tbs)?;
        if tag == VERSION {
            rest = tlv(rest)?.2; // Skip the serial number too.
        }
        rest = tlv(rest)?.2; // signature
        let (_, issuer, rest) = tlv(rest)?;
        let rest = tlv(rest)?.2; // validity
        let (_, subject, _) = tlv(rest)?;
        Some(issuer == subject)
    };
    fields().unwrap_or(false)
}

const SEQUENCE: u8 = 0x30;
const VERSION: u8 = 0xa0;
const UTC_TIME: u8 = 0x17;
const GENERALIZED_TIME: u8 = 0x18;

/// Splits one DER tag-length-value off the front: `(tag, value, rest)`.
fn tlv(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, input) = input.split_first()?;
    let (&first, mut input) = input.split_first()?;
    let len = if first < 0x80 {
        usize::from(first)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > 4 || input.len() < count {
            return None;
        }
        let (bytes, rest) = input.split_at(count);
        input = rest;
        bytes
            .iter()
            .fold(0_usize, |len, &byte| (len << 8) | usize::from(byte))
    };
    (input.len() >= len).then(|| {
        let (value, rest) = input.split_at(len);
        (tag, value, rest)
    })
}

/// Parses a `UTCTime` (`YYMMDDHHMMSSZ`) or `GeneralizedTime` (`YYYYMMDDHHMMSSZ`) to Unix seconds.
fn parse_time(tag: u8, time: &[u8]) -> Option<u64> {
    let digits = time.strip_suffix(b"Z")?;
    if !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let number = |range: std::ops::Range<usize>| {
        digits
            .get(range)?
            .iter()
            .try_fold(0_u64, |n, &d| Some(n * 10 + u64::from(d - b'0')))
    };
    let (year, rest) = match (tag, digits.len()) {
        (UTC_TIME, 12) => {
            let yy = number(0..2)?;
            (if yy >= 50 { 1900 + yy } else { 2000 + yy }, 2)
        }
        (GENERALIZED_TIME, 14) => (number(0..4)?, 4),
        _ => return None,
    };
    let month = number(rest..rest + 2)?;
    let day = number(rest + 2..rest + 4)?;
    let hour = number(rest + 4..rest + 6)?;
    let minute = number(rest + 6..rest + 8)?;
    let second = number(rest + 8..rest + 10)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    let days = days_from_civil(year, month, day)?;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second.min(60))
}

/// Days from 1970-01-01 to a date on or after it (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: u64, month: u64, day: u64) -> Option<u64> {
    let year = if month <= 2 {
        year.checked_sub(1)?
    } else {
        year
    };
    let era = year / 400;
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12; // March is 0.
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    (era * 146_097 + day_of_era).checked_sub(719_468)
}

/// Formats Unix seconds as `YYYY-MM-DDTHH:MM:SSZ`.
fn format_utc(seconds: u64) -> String {
    let (days, time) = (seconds / 86_400, seconds % 86_400);
    // Inverse of `days_from_civil`.
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3_600,
        time % 3_600 / 60,
        time % 60
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::io::Write as _;

    use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256, PKCS_ED25519};

    use super::*;

    struct Pair {
        cert: String,
        key: String,
    }

    fn pair_with(params: &CertificateParams, key: &KeyPair) -> Pair {
        let cert = params.self_signed(key).unwrap();
        Pair {
            cert: cert.pem(),
            key: key.serialize_pem(),
        }
    }

    fn pair() -> Pair {
        let key = KeyPair::generate().unwrap();
        pair_with(
            &CertificateParams::new(vec!["localhost".into()]).unwrap(),
            &key,
        )
    }

    fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        path
    }

    fn load_err(cert: &str, key: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let cert = write(dir.path(), "tls.crt", cert);
        let key = write(dir.path(), "tls.key", key);
        Certificates::load(cert, key).unwrap_err().to_string()
    }

    #[test]
    fn loads_a_matching_pair() {
        let dir = tempfile::tempdir().unwrap();
        let pair = pair();
        let certs = Certificates::load(
            write(dir.path(), "tls.crt", &pair.cert),
            write(dir.path(), "tls.key", &pair.key),
        )
        .unwrap();
        assert_eq!(certs.current().cert.len(), 1);
        assert!(!format!("{certs:?}").contains("PRIVATE"));
    }

    #[test]
    fn loads_every_supported_key_type() {
        for alg in [&PKCS_ECDSA_P256_SHA256, &PKCS_ED25519] {
            let dir = tempfile::tempdir().unwrap();
            let key = KeyPair::generate_for(alg).unwrap();
            let pair = pair_with(
                &CertificateParams::new(vec!["localhost".into()]).unwrap(),
                &key,
            );
            Certificates::load(
                write(dir.path(), "tls.crt", &pair.cert),
                write(dir.path(), "tls.key", &pair.key),
            )
            .unwrap();
        }
    }

    #[test]
    fn loads_a_chain_with_intermediates() {
        let dir = tempfile::tempdir().unwrap();
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let issuer = rcgen::Issuer::new(ca_params, &ca_key);
        let leaf_key = KeyPair::generate().unwrap();
        let leaf = CertificateParams::new(vec!["localhost".into()])
            .unwrap()
            .signed_by(&leaf_key, &issuer)
            .unwrap();
        let chain = format!("{}{}", leaf.pem(), ca.pem());
        let certs = Certificates::load(
            write(dir.path(), "tls.crt", &chain),
            write(dir.path(), "tls.key", &leaf_key.serialize_pem()),
        )
        .unwrap();
        assert_eq!(certs.current().cert.len(), 2);
    }

    #[test]
    fn refuses_bad_files() {
        let good = pair();
        let other = pair();
        let cases: [(&str, &str, &str); 6] = [
            (&good.cert, &other.key, "does not match"),
            ("", &good.key, "no PEM CERTIFICATE block"),
            (&good.key, &good.key, "no PEM CERTIFICATE block"),
            (&good.cert, "", "no unencrypted PEM private key"),
            (&good.cert, &good.cert, "no unencrypted PEM private key"),
            (
                &good.cert,
                "-----BEGIN ENCRYPTED PRIVATE KEY-----\nMAA=\n-----END ENCRYPTED PRIVATE KEY-----\n",
                "no unencrypted PEM private key",
            ),
        ];
        for (cert, key, expected) in cases {
            let error = load_err(cert, key);
            assert!(error.contains(expected), "{error:?} lacks {expected:?}");
        }
    }

    #[test]
    fn refuses_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let error = Certificates::load(dir.path().join("nope.crt"), dir.path().join("nope.key"))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("nope.crt"), "{error}");
    }

    #[test]
    fn reload_swaps_only_a_changed_valid_pair() {
        let dir = tempfile::tempdir().unwrap();
        let first = pair();
        let cert_path = write(dir.path(), "tls.crt", &first.cert);
        let key_path = write(dir.path(), "tls.key", &first.key);
        let certs = Certificates::load(&cert_path, &key_path).unwrap();
        let original = certs.current();

        assert!(!certs.reload().unwrap(), "unchanged files must not swap");
        assert!(Arc::ptr_eq(&original, &certs.current()));

        // A renewal that writes the certificate first: refused until the key follows.
        let second = pair();
        write(dir.path(), "tls.crt", &second.cert);
        assert!(
            certs
                .reload()
                .unwrap_err()
                .to_string()
                .contains("does not match")
        );
        assert!(Arc::ptr_eq(&original, &certs.current()));
        write(dir.path(), "tls.key", &second.key);
        assert!(certs.reload().unwrap());
        assert_ne!(original.cert, certs.current().cert);

        // Files that vanish keep the current certificate.
        std::fs::remove_file(&key_path).unwrap();
        assert!(certs.reload().is_err());
        let expected = CertificateDer::from_pem_slice(second.cert.as_bytes()).unwrap();
        assert_eq!(certs.current().cert[0], expected);
    }

    #[test]
    fn generates_a_self_signed_pair_once() {
        let dir = tempfile::tempdir().unwrap();
        // A directory that doesn't exist yet, as on a fresh volume.
        let cert_path = dir.path().join("tls").join("tls.crt");
        let key_path = dir.path().join("tls").join("tls.key");
        assert!(both_missing(&cert_path, &key_path).unwrap());
        assert!(generate_if_missing(&cert_path, &key_path, &[]).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&key_path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(!dir.path().join("tls").join("tls.key.tmp").exists());

        let certs = Certificates::load(&cert_path, &key_path).unwrap();
        let leaf = certs.current().cert[0].clone();
        assert!(is_self_signed(&leaf));
        assert!(contains(&leaf, b"mammoth-cache"));
        assert!(contains(&leaf, b"localhost"));
        assert!(contains(&leaf, &[127, 0, 0, 1]));
        assert!(contains(&leaf, &std::net::Ipv6Addr::LOCALHOST.octets()));
        let expiry = not_after(&leaf).unwrap();
        let expected = unix_now() + GENERATED_VALIDITY.as_secs();
        assert!(expiry.abs_diff(expected) < 60, "{}", format_utc(expiry));
        assert_eq!(fingerprint(&leaf).len(), 32 * 3 - 1);

        // A restart keeps the files byte for byte.
        let before = (
            std::fs::read(&cert_path).unwrap(),
            std::fs::read(&key_path).unwrap(),
        );
        assert!(!generate_if_missing(&cert_path, &key_path, &[]).unwrap());
        let after = (
            std::fs::read(&cert_path).unwrap(),
            std::fs::read(&key_path).unwrap(),
        );
        assert_eq!(before, after);
    }

    #[test]
    fn generates_for_the_given_hostnames() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("tls.crt");
        let key_path = dir.path().join("tls.key");
        let names = ["cache.example.com".to_owned(), "10.1.2.3".to_owned()];
        generate_if_missing(&cert_path, &key_path, &names).unwrap();
        let leaf = Certificates::load(&cert_path, &key_path)
            .unwrap()
            .current()
            .cert[0]
            .clone();
        assert!(contains(&leaf, b"cache.example.com"));
        assert!(contains(&leaf, &[10, 1, 2, 3]));
        assert!(!contains(&leaf, b"localhost"));
    }

    #[test]
    fn refuses_to_generate_beside_half_a_pair() {
        let dir = tempfile::tempdir().unwrap();
        let pair = pair();
        let cert_path = dir.path().join("tls.crt");
        let key_path = write(dir.path(), "tls.key", &pair.key);
        for _ in 0..2 {
            let error = generate_if_missing(&cert_path, &key_path, &[]).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::NotFound);
            let message = error.to_string();
            assert!(message.contains("tls.key exists but"), "{message}");
            assert!(message.contains("delete both"), "{message}");
            assert_eq!(std::fs::read_to_string(&key_path).unwrap(), pair.key);
            assert!(!cert_path.exists());
            // The same with the other file present.
            std::fs::remove_file(&key_path).unwrap();
            write(dir.path(), "tls.crt", &pair.cert);
            let error = both_missing(&cert_path, &key_path).unwrap_err();
            assert!(error.to_string().contains("tls.crt exists but"), "{error}");
            std::fs::remove_file(&cert_path).unwrap();
            write(dir.path(), "tls.key", &pair.key);
        }
    }

    #[test]
    fn tells_self_signed_from_issued() {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "test CA");
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let issuer = rcgen::Issuer::new(ca_params, &ca_key);
        let leaf_key = KeyPair::generate().unwrap();
        let mut leaf_params = CertificateParams::new(vec!["localhost".into()]).unwrap();
        leaf_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "leaf");
        let leaf = leaf_params.signed_by(&leaf_key, &issuer).unwrap();
        assert!(is_self_signed(ca.der()));
        assert!(!is_self_signed(leaf.der()));
        assert!(!is_self_signed(b""));
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    #[test]
    fn reads_not_after() {
        let key = KeyPair::generate().unwrap();
        for (year, month, day) in [(2049, 12, 31), (2050, 1, 1), (2000, 2, 29), (1999, 3, 1)] {
            let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
            params.not_after = rcgen::date_time_ymd(year, month, day);
            let cert = params.self_signed(&key).unwrap();
            let expiry = not_after(cert.der()).unwrap();
            assert_eq!(
                format_utc(expiry),
                format!("{year:04}-{month:02}-{day:02}T00:00:00Z")
            );
        }
        assert_eq!(not_after(b""), None);
        assert_eq!(not_after(&[0x30, 0x82, 0xff]), None);
    }

    #[test]
    fn parses_both_time_forms() {
        assert_eq!(parse_time(UTC_TIME, b"700101000000Z"), Some(0));
        assert_eq!(parse_time(UTC_TIME, b"491231235959Z"), Some(2_524_607_999));
        assert_eq!(
            parse_time(GENERALIZED_TIME, b"20500101000000Z"),
            Some(2_524_608_000)
        );
        assert_eq!(parse_time(UTC_TIME, b"20500101000000Z"), None);
        assert_eq!(parse_time(UTC_TIME, b"701301000000Z"), None);
        assert_eq!(parse_time(UTC_TIME, b"7001010000Z0"), None);
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
    }
}
