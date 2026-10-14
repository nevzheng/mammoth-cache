use std::{
    env, io,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};

use mammoth_cache::{
    auth,
    cache::{BucketLimits, BucketWrite, Cache},
    config::{self, Config, DEFAULT_PATH, LogFormat, PATH_ENV},
    server::{self, ServerSettings, TlsSettings},
    store::{
        bucket::{BucketConfig, BucketStore},
        disk::{DiskConfig, DiskStore},
        memory::{MemoryConfig, MemoryStore},
    },
    telemetry,
    tls::{self, Certificates},
};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

// The global allocator is chosen at build time (see the features in Cargo.toml).
#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(all(feature = "mimalloc", not(feature = "jemalloc")))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const USAGE: &str = "\
usage: mammoth [--config <path>]
       mammoth config check [--config <path>]
       mammoth token new

The config file is --config, else $MAMMOTH_CONFIG, else ./mammoth.toml.";

enum Command {
    Serve,
    Check,
    NewToken,
}

fn main() -> ExitCode {
    let (command, explicit) = match parse_args(env::args().skip(1)) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if matches!(command, Command::NewToken) {
        return new_token();
    }
    let (config, source) = match load_config(explicit) {
        Ok(loaded) => loaded,
        Err(message) => {
            eprintln!("mammoth: {message}");
            return ExitCode::FAILURE;
        }
    };
    if matches!(command, Command::Check) {
        // Check what the file points at too, without the startup log.
        if let Err(error) = check(&config) {
            eprintln!("mammoth: {error}");
            return ExitCode::FAILURE;
        }
        eprintln!("config ok");
        return ExitCode::SUCCESS;
    }

    init_logging(&config);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("mammoth: cannot start the runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(path) = source {
        tracing::info!(path = %path.display(), "config loaded");
    } else {
        tracing::info!("no {DEFAULT_PATH} found; using the defaults");
    }
    match runtime.block_on(serve(config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "mammoth-cache stopped");
            ExitCode::FAILURE
        }
    }
}

fn parse_args(
    mut args: impl Iterator<Item = String>,
) -> Result<(Command, Option<PathBuf>), String> {
    let mut command = Command::Serve;
    let mut config = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                let path = args.next().ok_or("--config needs a path")?;
                config = Some(PathBuf::from(path));
            }
            "config" if matches!(command, Command::Serve) => match args.next().as_deref() {
                Some("check") => command = Command::Check,
                _ => return Err("expected `config check`".to_owned()),
            },
            "token" if matches!(command, Command::Serve) => match args.next().as_deref() {
                Some("new") => command = Command::NewToken,
                _ => return Err("expected `token new`".to_owned()),
            },
            "-h" | "--help" => return Err("mammoth-cache, a remote build cache node".to_owned()),
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    Ok((command, config))
}

/// Loads the config file. With no `--config` or `$MAMMOTH_CONFIG` and no `./mammoth.toml`, the
/// node runs on the defaults, memory only, so `docker run` works without a file.
/// Prints a new token and the hash to put in `mammoth.toml`.
#[allow(clippy::print_stdout)] // The command's output.
fn new_token() -> ExitCode {
    match auth::new_token() {
        Ok(token) => {
            let hash = auth::format_hash(&auth::hash_token(&token));
            println!("token: {token}");
            println!("hash:  {hash}");
            eprintln!(
                "\nGive the token to the client, add the hash to the user's token_hashes in \
                 mammoth.toml, and restart. The token is not stored anywhere."
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("mammoth: cannot generate a token: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Returns the config and the file it came from, if any.
fn load_config(explicit: Option<PathBuf>) -> Result<(Config, Option<PathBuf>), String> {
    let chosen = explicit.is_some() || env::var_os(PATH_ENV).is_some();
    let path = Config::locate(explicit);
    if !chosen && !path.exists() {
        return Ok((Config::fallback(), None));
    }
    let config = Config::load(&path).map_err(|e| e.to_string())?;
    Ok((config, Some(path)))
}

fn init_logging(config: &Config) {
    // RUST_LOG still wins, for debugging one module.
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| config.log.level.as_str().into());
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match config.log.format {
        LogFormat::Json => builder.json().init(),
        LogFormat::Text => builder.init(),
    }
}

async fn serve(config: Config) -> io::Result<()> {
    let auth = config.auth.authenticator(&config.metrics);
    // Before anything else, so a bad certificate fails fast.
    let certificates = match &config.tls {
        Some(tls) => Some(Arc::new(Certificates::load(&tls.cert, &tls.key)?)),
        None => None,
    };
    if !config.auth.is_configured(&config.metrics) {
        tracing::warn!("no [auth] users configured: every request is allowed");
    } else if config.auth.grants_nothing() {
        tracing::warn!(
            "[auth] has no users and no anonymous scopes: every request except health checks \
             is refused"
        );
    }
    if config.auth.is_configured(&config.metrics) && certificates.is_none() {
        tracing::warn!(
            "authentication is on and this server speaks plain HTTP: set [tls] or serve it \
             behind a proxy that terminates TLS, or tokens cross the network in clear"
        );
    }
    // Before the tiers open, so what they record while opening is kept.
    let metrics = telemetry::install().map_err(io::Error::other)?;
    let cache = build_cache(&config).await?;
    let protocols = &config.protocols;
    tracing::info!(
        gradle = protocols.gradle,
        maven = protocols.maven,
        bazel = protocols.bazel,
        "protocols"
    );
    let listener = TcpListener::bind(config.listen).await?;
    tracing::info!(
        addr = %listener.local_addr()?,
        scheme = if certificates.is_some() { "https" } else { "http" },
        "mammoth-cache listening"
    );

    let cache = Arc::new(cache);
    let app = mammoth_cache::router_with(
        cache.clone(),
        Some(metrics),
        Arc::new(auth),
        config.protocols,
    );
    let settings = ServerSettings {
        shutdown_delay: config.server.shutdown_delay.0,
        shutdown_timeout: config.server.shutdown_timeout.0,
        idle_timeout: config.server.idle_timeout.0,
        body_read_timeout: config.server.body_read_timeout.0,
        send_timeout: config.server.send_timeout.0,
    };
    let signal = async {
        shutdown_signal().await;
        // A second signal skips the drain.
        tokio::spawn(async {
            shutdown_signal().await;
            tracing::warn!("second shutdown signal; exiting without draining");
            std::process::exit(130);
        });
    };
    match certificates {
        Some(certificates) => {
            let tls = TlsSettings {
                config: certificates.server_config()?,
                handshake_timeout: tls::HANDSHAKE_TIMEOUT,
            };
            tokio::spawn(certificates.watch(tls::RELOAD_INTERVAL));
            server::serve_tls(listener, tls, app, cache, settings, signal).await
        }
        None => server::serve(listener, app, cache, settings, signal).await,
    }
}

/// Builds the tiers in lookup order: memory, then disk, then the bucket.
async fn build_cache(config: &Config) -> io::Result<Cache> {
    let memory: MemoryStore = MemoryStore::with_config(MemoryConfig {
        capacity_bytes: config.memory.size.bytes(),
        max_entry_bytes: config.memory.max_entry_size.bytes(),
        eviction: config.memory.eviction,
    })
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    // With memory as the deepest tier, the cache caps entries at memory.max_entry_size.
    let mut builder = Cache::builder()
        .max_entry_size(config.limits.max_entry_size.bytes())
        .bounded_tier(Arc::new(memory));
    if let Some(disk) = &config.disk {
        let store = DiskStore::open(DiskConfig {
            path: disk.path.clone(),
            capacity_bytes: disk.size.bytes(),
            max_entry_age: disk.max_entry_age.map(|age| age.0),
            free_space_buffer: disk.free_space_buffer.bytes(),
        })
        .await?;
        builder = builder.bounded_tier(Arc::new(store));
    } else {
        tracing::warn!("no [disk] configured: running memory only, entries are lost on restart");
    }
    if let Some(bucket) = &config.bucket {
        let store = BucketStore::open(&bucket_config(bucket))?;
        let write = match bucket.write {
            config::BucketWrite::Async => BucketWrite::Async {
                queue: bucket.upload_queue,
            },
            config::BucketWrite::Sync => BucketWrite::Sync,
        };
        builder = builder
            .bucket_tier(Arc::new(store), write)
            .bucket_limits(BucketLimits {
                timeout: bucket.timeout.0,
                cooldown: bucket.cooldown.0,
            });
    }
    Ok(builder.build())
}

fn bucket_config(bucket: &config::Bucket) -> BucketConfig {
    BucketConfig {
        region: bucket.region.clone(),
        endpoint: bucket.endpoint.clone(),
        allow_http: bucket.allow_http,
        timeout: bucket.timeout.0,
        retries: bucket.retries,
        ..BucketConfig::new(bucket.url.clone())
    }
}

/// What `config check` can find without starting the node: a certificate that doesn't load, a
/// bucket URL no client accepts, or a disk path that can't be used. It contacts nothing and
/// writes nothing.
fn check(config: &Config) -> Result<(), String> {
    if let Some(tls) = &config.tls {
        Certificates::load(&tls.cert, &tls.key).map_err(|e| e.to_string())?;
    }
    if let Some(bucket) = &config.bucket {
        BucketStore::open(&bucket_config(bucket)).map_err(|e| format!("bucket.url: {e}"))?;
    }
    if let Some(disk) = &config.disk {
        check_disk_path(&disk.path)
            .map_err(|e| format!("disk.path {}: {e}", disk.path.display()))?;
    }
    Ok(())
}

/// The disk path, or the nearest parent that exists if it doesn't yet, is a writable directory.
fn check_disk_path(path: &Path) -> io::Result<()> {
    let mut existing = path;
    while !existing.exists() {
        match existing.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => existing = parent,
            _ => return Ok(()),
        }
    }
    if !existing.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{} is not a directory", existing.display()),
        ));
    }
    #[cfg(unix)]
    rustix::fs::access(existing, rustix::fs::Access::WRITE_OK).map_err(|e| {
        io::Error::new(
            io::Error::from(e).kind(),
            format!("{} is not writable", existing.display()),
        )
    })?;
    Ok(())
}

/// Resolves on Ctrl-C or, on Unix, `SIGTERM`, which is how Kubernetes and Docker stop a container.
/// As PID 1 the process gets no default `SIGTERM` handler, so without this it would be killed only
/// after the grace period.
async fn shutdown_signal() {
    // If a handler can't be installed, wait on the other rather than exit at once.
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    let name = tokio::select! {
        () = ctrl_c => "interrupt",
        () = terminate => "SIGTERM",
    };
    tracing::info!(signal = name, "shutdown signal received");
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn config(dir: &Path, extra: &str) -> Config {
        let file = dir.join("mammoth.toml");
        std::fs::write(&file, format!("version = 1\n{extra}")).unwrap();
        Config::load(&file).unwrap()
    }

    #[test]
    fn check_finds_what_would_fail_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a-file");
        std::fs::write(&file, b"").unwrap();

        let cases = [
            ("[bucket]\nurl = \"ftp://host/x\"\n", "bucket.url"),
            (
                &format!("[disk]\npath = \"{}\"\nsize = \"1GiB\"\n", file.display()),
                "not a directory",
            ),
            (
                &format!(
                    "[disk]\npath = \"{}\"\nsize = \"1GiB\"\n",
                    file.join("below").display()
                ),
                "not a directory",
            ),
        ];
        for (extra, expected) in cases {
            let err = check(&config(dir.path(), extra)).unwrap_err();
            assert!(err.contains(expected), "{extra:?}: {err}");
        }
    }

    #[test]
    fn check_accepts_a_disk_path_that_does_not_exist_yet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not/yet");
        let extra = format!(
            "[disk]\npath = \"{}\"\nsize = \"1GiB\"\n[bucket]\nurl = \"file://{}\"\n",
            path.display(),
            dir.path().display()
        );
        check(&config(dir.path(), &extra)).unwrap();
        assert!(!path.exists(), "config check created the disk path");
    }
}
