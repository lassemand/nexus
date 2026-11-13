//! The analyst service: applies its migrations, runs the scheduler that starts
//! the preparation of a kernel before each upcoming report, and consumes report
//! notes. The launcher is a no-op until the Job launcher exists; the release
//! path and trades come in later tasks.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use analyst::metrics::{LOAD_BUCKETS, LOAD_SECONDS, RUN_BUCKETS, RUN_SECONDS};
use analyst::notes::run_consumer;
use analyst::{
    Config, KernelRuntime, NoopLauncher, NoteHandler, RuntimeConfig, Scheduler, SchedulerConfig,
    SignalSchedule, Store, SystemClock,
};
use anyhow::{Context, Result};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::sync::{watch, Notify};
use tracing::{error, info};

const DEFAULT_METRICS_PORT: u16 = 9464;

async fn stopped(mut stop: watch::Receiver<bool>) {
    let _ = stop.wait_for(|stopping| *stopping).await;
}

async fn wait_for_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut terminate =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(terminate) => terminate,
                Err(error) => {
                    error!(%error, "cannot listen for SIGTERM");
                    let _ = ctrl_c.await;
                    return;
                }
            };
        tokio::select! {
            _ = ctrl_c => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}

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
    analyst::metrics::describe();

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&config.database_url)
        .await
        .context("cannot connect to the analyst database")?;
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("cannot apply the analyst migrations")?;
    let signal_options = PgConnectOptions::from_str(&config.signal_database_url)?
        .options([("default_transaction_read_only", "on")]);
    let signal_pool = PgPoolOptions::new()
        .max_connections(3)
        .acquire_timeout(Duration::from_secs(5))
        .connect_lazy_with(signal_options);

    let _runtime = KernelRuntime::new(RuntimeConfig {
        deadline: config.kernel_deadline,
        ..RuntimeConfig::default()
    })?;

    let clock = Arc::new(SystemClock);
    let wake = Arc::new(Notify::new());
    let store = Store::new(pool);
    let signal = SignalSchedule::new(signal_pool, config.analyst_tickers.clone());
    let scheduler = Scheduler::new(
        store.clone(),
        signal.clone(),
        SchedulerConfig::from(&config),
        clock.clone(),
        Arc::new(NoopLauncher),
    );
    let notes = NoteHandler::new(store, signal, clock, wake.clone());
    info!(
        prep_lead = ?config.prep_lead,
        tick = ?config.scheduler_tick,
        allowlist = config.analyst_tickers.len(),
        topic = %config.notes_topic,
        "analyst starting"
    );

    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        wait_for_signal().await;
        let _ = stop_tx.send(true);
    });
    let consumer = async {
        if let Err(error) = run_consumer(
            &config.kafka_brokers,
            &config.notes_topic,
            &notes,
            stopped(stop_rx.clone()),
        )
        .await
        {
            error!(%error, "notes consumer stopped");
        }
    };
    tokio::join!(scheduler.run(wake, stopped(stop_rx.clone())), consumer);
    info!("analyst stopped");
    Ok(())
}
