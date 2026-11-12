//! The analyst service. So far it applies its migrations and holds the kernel
//! runtime; the scheduler, notes consumer, preparation job and release
//! consumers come in later tasks.

use analyst::metrics::{LOAD_BUCKETS, LOAD_SECONDS, RUN_BUCKETS, RUN_SECONDS};
use analyst::{Config, KernelRuntime, RuntimeConfig};
use anyhow::{Context, Result};
use sqlx::postgres::PgPoolOptions;

const DEFAULT_METRICS_PORT: u16 = 9464;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let config = Config::from_env().context("invalid configuration")?;
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

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&config.database_url)
        .await
        .context("cannot connect to the analyst database")?;
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("cannot apply the analyst migrations")?;

    let _runtime = KernelRuntime::new(RuntimeConfig {
        deadline: config.kernel_deadline,
        ..RuntimeConfig::default()
    })?;
    tracing::info!(deadline = ?config.kernel_deadline, port, "analyst ready");
    tokio::signal::ctrl_c().await?;
    tracing::info!("analyst stopping");
    Ok(())
}
