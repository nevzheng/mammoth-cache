use std::{env, io};

use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

const DEFAULT_ADDR: &str = "0.0.0.0:5071";

#[tokio::main]
async fn main() -> io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let addr = env::var("MAMMOTH_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_owned());
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!(addr = %listener.local_addr()?, "mammoth-cache listening");

    axum::serve(listener, mammoth_cache::router())
        .with_graceful_shutdown(shutdown_signal())
        .await
}

async fn shutdown_signal() {
    // If the handler can't be installed, run until killed rather than exit at once.
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}
