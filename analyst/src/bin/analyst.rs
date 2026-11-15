//! The analyst service: applies its migrations, runs the scheduler that starts
//! the preparation of a kernel before each upcoming report, consumes report
//! notes, and decides on each report release with the prepared kernel. The
//! launcher and the trade scheduler are no-ops until they exist.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use analyst::metrics::{
    ENTRY_DELAY_BUCKETS, ENTRY_DELAY_SECONDS, LATENESS_BUCKETS, LOAD_BUCKETS, LOAD_SECONDS,
    PUBLISH_BUCKETS, PUBLISH_TO_DECISION_SECONDS, QUOTE_LATENESS_SECONDS, RELEASE_BUCKETS,
    RELEASE_TO_DECISION_SECONDS, RUN_BUCKETS, RUN_SECONDS,
};
use analyst::notes::run_consumer;
use analyst::{
    preload, run_release_consumer, Config, HttpQuoteSource, KafkaDecisionSink, KafkaTradeSink,
    KernelRegistry, KernelRuntime, NoopLauncher, NoteHandler, ReleaseProcessor, RunnerSettings,
    RuntimeConfig, Scheduler, SchedulerConfig, SessionsCache, SignalSchedule, Store, SystemClock,
    TradeRunner,
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
    let _tracing = telemetry::trace::init("analyst").context("cannot start tracing")?;
    let config = Config::from_env().context("invalid configuration")?;
    let port = std::env::var("METRICS_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_METRICS_PORT);
    telemetry::serve_with_buckets(
        "analyst",
        port,
        &[
            (RUN_SECONDS, RUN_BUCKETS),
            (LOAD_SECONDS, LOAD_BUCKETS),
            (RELEASE_TO_DECISION_SECONDS, RELEASE_BUCKETS),
            (PUBLISH_TO_DECISION_SECONDS, PUBLISH_BUCKETS),
            (ENTRY_DELAY_SECONDS, ENTRY_DELAY_BUCKETS),
            (QUOTE_LATENESS_SECONDS, LATENESS_BUCKETS),
        ],
    )
    .await?;
    analyst::metrics::describe();
    analyst::metrics::describe_releases();
    analyst::metrics::describe_trades();

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

    let runtime = Arc::new(KernelRuntime::new(RuntimeConfig {
        deadline: config.kernel_deadline,
        ..RuntimeConfig::default()
    })?);

    let clock = Arc::new(SystemClock);
    let wake = Arc::new(Notify::new());
    let store = Store::new(pool);
    let calendar_pool = signal_pool.clone();
    let signal = SignalSchedule::new(signal_pool, config.analyst_tickers.clone());
    let scheduler = Scheduler::new(
        store.clone(),
        signal.clone(),
        SchedulerConfig::from(&config),
        clock.clone(),
        Arc::new(NoopLauncher),
    );
    let notes = NoteHandler::new(store.clone(), signal, clock.clone(), wake.clone());

    let registry = Arc::new(KernelRegistry::new());
    preload(&store, &runtime, &registry)
        .await
        .context("cannot load the accepted kernels")?;
    let sink = KafkaDecisionSink::new(&config.kafka_brokers, &config.decisions_topic)
        .context("cannot create the decisions producer")?;
    let sessions = SessionsCache::default();
    sessions.reload(&calendar_pool).await;
    let trade_sink = KafkaTradeSink::new(&config.kafka_brokers, &config.trades_topic)
        .context("cannot create the trades producer")?;
    let quotes = HttpQuoteSource::new(&config.saxo_quotes_url)
        .map_err(|error| anyhow::anyhow!("cannot create the quote client: {error}"))?;
    let runner = TradeRunner::new(
        Arc::new(store.clone()),
        sessions.clone(),
        Arc::new(quotes),
        Arc::new(trade_sink),
        clock.clone(),
        RunnerSettings::from_config(&config),
    );
    let resumed = runner
        .resume()
        .await
        .context("cannot reschedule the open simulated trades")?;
    info!(resumed, "open simulated trades rescheduled");
    let releases = Arc::new(
        ReleaseProcessor::new(
            store,
            Arc::clone(&registry),
            clock,
            Arc::new(sink),
            Arc::new(runner),
            config.max_release_age,
        )
        .with_decisions_topic(config.decisions_topic.clone()),
    );
    info!(
        prep_lead = ?config.prep_lead,
        tick = ?config.scheduler_tick,
        allowlist = config.analyst_tickers.len(),
        topic = %config.notes_topic,
        releases_topic = %config.special_events_topic,
        decisions_topic = %config.decisions_topic,
        trades_topic = %config.trades_topic,
        hold = ?config.hold,
        max_release_age = ?config.max_release_age,
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
    let release_consumer = async {
        if let Err(error) = run_release_consumer(
            &config.kafka_brokers,
            &config.special_events_topic,
            releases,
            stopped(stop_rx.clone()),
        )
        .await
        {
            error!(%error, "release consumer stopped");
        }
    };
    let calendars = sessions.run_reload(calendar_pool, stopped(stop_rx.clone()));
    tokio::join!(
        scheduler.run(wake, stopped(stop_rx.clone())),
        consumer,
        release_consumer,
        calendars
    );
    info!("analyst stopped");
    Ok(())
}
