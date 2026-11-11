//! The analyst service. This task adds the kernel runtime only; the database,
//! scheduler, preparation job and release consumers come in later tasks.

use analyst::metrics::{LOAD_BUCKETS, LOAD_SECONDS, RUN_BUCKETS, RUN_SECONDS};
use analyst::{KernelRuntime, RuntimeConfig};
use anyhow::Result;

const DEFAULT_METRICS_PORT: u16 = 9464;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let port = std::env::var("METRICS_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_METRICS_PORT);
    telemetry::serve_with_buckets(
        "analyst",
        port,
        &[(RUN_SECONDS, RUN_BUCKETS), (LOAD_SECONDS, LOAD_BUCKETS)],
    )
    .await?;
    let config = RuntimeConfig::from_env();
    let _runtime = KernelRuntime::new(config)?;
    tracing::info!(deadline = ?config.deadline, port, "analyst kernel runtime ready");
    tokio::signal::ctrl_c().await?;
    tracing::info!("analyst stopping");
    Ok(())
}
