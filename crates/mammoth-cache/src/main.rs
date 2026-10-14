use std::{env, io, sync::Arc};

use mammoth_cache::{cache::Cache, store::memory::MemoryStore};
use metrics_exporter_prometheus::PrometheusBuilder;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

// The global allocator is chosen at build time (see the features in Cargo.toml).
#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(all(feature = "mimalloc", not(feature = "jemalloc")))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const DEFAULT_ADDR: &str = "0.0.0.0:5071";
// Until the config file (design doc 0005) lands, the memory tier is the only tier.
const DEFAULT_MEMORY_BYTES: u64 = 1024 * 1024 * 1024;

#[tokio::main]
async fn main() -> io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let addr = env::var("MAMMOTH_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_owned());
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!(addr = %listener.local_addr()?, "mammoth-cache listening");

    let cache = Cache::builder()
        .bounded_tier(Arc::new(MemoryStore::new(DEFAULT_MEMORY_BYTES)))
        .build();
    let metrics = PrometheusBuilder::new()
        .install_recorder()
        .map_err(io::Error::other)?;
    axum::serve(
        listener,
        mammoth_cache::router(Arc::new(cache), Some(metrics)),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
}

async fn shutdown_signal() {
    // If the handler can't be installed, run until killed rather than exit at once.
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}
