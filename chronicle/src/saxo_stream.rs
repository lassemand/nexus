/// Persistent Saxo Bank WebSocket streaming ingestion service.
///
/// # Architectural note
///
/// This is the **only non-CronJob binary in the `chronicle` crate**. Every other
/// binary runs to completion (one-shot CronJob). A WebSocket stream requires a
/// long-running process — this binary is deployed as a Kubernetes `Deployment`
/// with a single replica.
///
/// # Data flow
///
/// ```text
/// companies table (FNSE tickers)
///         │  (refresh every TICKER_REFRESH_INTERVAL_SECS)
///         ▼
/// Uic resolver (Saxo /ref/v1/instruments, Stock-only)
///         │
///         ▼
/// Saxo WebSocket stream → BarAggregator (1-min OHLCV)
///         │
///         ▼
/// ChronicleProducer → market.bars (same Kafka topic as Polygon bars)
/// ```
///
/// # Health endpoint
///
/// `GET /health` on `HEALTH_PORT` (default 8080) returns:
/// - `200 OK` — WebSocket connected and receiving heartbeats
/// - `503 Service Unavailable` — WebSocket disconnected or unhealthy
///
/// # Token lifecycle
///
/// The Saxo OAuth2 access token is refreshed in-place via
/// `PUT /ws/authorize?contextid=<id>` without reconnecting (per ADR-0001).
///
/// # Ticker refresh
///
/// The service re-reads FNSE tickers from the `companies` table every
/// `TICKER_REFRESH_INTERVAL_SECS`. New tickers registered via
/// `nexus register GOMX.ST` are picked up without a restart.
mod db;
mod kafka;

use alpha::saxo::{SaxoBarStream, SaxoConfig, SaxoToken, UicResolver};
use anyhow::Context;
use chrono::Utc;
use clap::Parser;
use kafka::ChronicleProducer;
use sqlx::postgres::PgPoolOptions;
use sqlx::Row;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::watch;
use tokio::time::{interval, Duration};
use tracing::{error, info, warn};

// ── Args ──────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    about = "Persistent Saxo WebSocket streaming ingestion service — Deployment, not CronJob"
)]
struct Args {
    /// Postgres connection URL (reads FNSE tickers from companies table).
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    /// Kafka broker addresses.
    #[arg(long, env = "KAFKA_BROKERS")]
    kafka_brokers: String,

    /// Kafka topic for published bars (same as Polygon bars — signal unchanged).
    #[arg(long, env = "KAFKA_TOPIC", default_value = "market.bars")]
    kafka_topic: String,

    /// Saxo Bank REST API base URL.
    #[arg(
        long,
        env = "SAXO_API_BASE",
        default_value = "https://gateway.saxobank.com/openapi"
    )]
    saxo_api_base: String,

    /// Saxo Bank streaming WebSocket base URL.
    #[arg(
        long,
        env = "SAXO_STREAMING_BASE",
        default_value = "https://live-streaming.saxobank.com/oapi/streaming/ws"
    )]
    saxo_streaming_base: String,

    /// Saxo OAuth2 access token (initial). The service refreshes it automatically.
    #[arg(long, env = "SAXO_ACCESS_TOKEN")]
    saxo_access_token: String,

    /// Saxo OAuth2 token expiry as a Unix timestamp (seconds).
    #[arg(long, env = "SAXO_TOKEN_EXPIRES_AT")]
    saxo_token_expires_at: i64,

    /// How often to re-read the FNSE ticker list from the companies table.
    #[arg(long, env = "TICKER_REFRESH_INTERVAL_SECS", default_value = "300")]
    ticker_refresh_interval_secs: u64,

    /// TCP port for the HTTP health endpoint.
    #[arg(long, env = "HEALTH_PORT", default_value = "8080")]
    health_port: u16,

    /// OHLCV bar aggregation window in seconds.
    #[arg(long, env = "BAR_WINDOW_SECS", default_value = "60")]
    bar_window_secs: i64,
}

// ── Ticker registry ───────────────────────────────────────────────────────

/// Load all FNSE-tagged tickers from the companies table.
async fn load_fnse_tickers(pool: &sqlx::PgPool) -> anyhow::Result<Vec<String>> {
    let rows =
        sqlx::query("SELECT ticker FROM companies WHERE exchange_mic = 'FNSE' ORDER BY ticker")
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|r| r.get("ticker")).collect())
}

// ── Health server ─────────────────────────────────────────────────────────

/// Serve a minimal HTTP health endpoint.
///
/// Returns 200 when `healthy` is true, 503 otherwise.
/// k8s liveness probe target: `GET /health`
async fn serve_health(port: u16, healthy: Arc<AtomicBool>) {
    let listener = match TcpListener::bind(format!("0.0.0.0:{port}")).await {
        Ok(l) => l,
        Err(e) => {
            error!(port, error = %e, "failed to bind health endpoint");
            return;
        }
    };
    info!(port, "health endpoint listening");

    loop {
        match listener.accept().await {
            Err(e) => {
                warn!(error = %e, "health endpoint accept error");
                continue;
            }
            Ok((mut stream, _)) => {
                let ok = healthy.load(Ordering::Relaxed);
                let (status, body) = if ok {
                    ("200 OK", "ok")
                } else {
                    ("503 Service Unavailable", "unhealthy")
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        }
    }
}

// ── Main ──────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/.env")).ok();
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    // ── DB pool ───────────────────────────────────────────────────────────
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .connect(&args.database_url)
        .await
        .context("failed to connect to postgres")?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("migrations failed")?;

    // ── Kafka producer ────────────────────────────────────────────────────
    let producer =
        ChronicleProducer::new(&args.kafka_brokers).context("failed to create Kafka producer")?;

    // ── Health flag ───────────────────────────────────────────────────────
    let healthy = Arc::new(AtomicBool::new(false));
    let health_flag = healthy.clone();
    tokio::spawn(serve_health(args.health_port, health_flag));

    // ── Shutdown signal ───────────────────────────────────────────────────
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let shutdown_tx = Arc::new(shutdown_tx);

    {
        let tx = shutdown_tx.clone();
        tokio::spawn(async move {
            signal::ctrl_c().await.ok();
            info!("SIGTERM/SIGINT received — initiating graceful shutdown");
            let _ = tx.send(true);
        });
    }

    // ── Initial ticker load ───────────────────────────────────────────────
    let tickers = load_fnse_tickers(&pool).await.unwrap_or_default();
    if tickers.is_empty() {
        warn!(
            "no FNSE tickers registered — register via: nexus register GOMX.ST\n\
             Service will poll for tickers every {} seconds",
            args.ticker_refresh_interval_secs
        );
    } else {
        info!(count = tickers.len(), tickers = ?tickers, "loaded FNSE tickers");
    }

    // ── HTTP client + Uic resolver ────────────────────────────────────────
    let http = reqwest::Client::builder()
        .user_agent("nexus lasse.alm@gsfleet.io")
        .build()
        .context("failed to build HTTP client")?;

    let uic_resolver = UicResolver::new(http.clone(), &args.saxo_api_base);

    // ── Saxo config ───────────────────────────────────────────────────────
    let config = SaxoConfig {
        api_base: args.saxo_api_base.clone(),
        streaming_base: args.saxo_streaming_base.clone(),
        context_id: format!("nexus-chronicle-{}", Utc::now().timestamp()),
        bar_window_secs: args.bar_window_secs,
        max_backoff_secs: 60,
        token_refresh_threshold_secs: 120,
        heartbeat_timeout_secs: 30,
    };

    let token = SaxoToken {
        access_token: args.saxo_access_token.clone(),
        expires_at: chrono::DateTime::from_timestamp(args.saxo_token_expires_at, 0)
            .unwrap_or_else(Utc::now),
    };

    // ── Ticker refresh interval ───────────────────────────────────────────
    let mut refresh_ticker = interval(Duration::from_secs(args.ticker_refresh_interval_secs));
    refresh_ticker.tick().await; // consume the immediate first tick

    // ── Stream loop ───────────────────────────────────────────────────────
    info!("starting Saxo stream ingestion loop");
    let mut current_tickers = tickers;
    let mut stream_opt: Option<SaxoBarStream> = None;

    loop {
        // Check for shutdown.
        if *shutdown_rx.borrow() {
            info!("shutdown signal received — flushing and exiting");
            break;
        }

        // Refresh ticker list periodically (non-blocking poll).
        if tokio::time::timeout(Duration::from_millis(1), refresh_ticker.tick())
            .await
            .is_ok()
        {
            match load_fnse_tickers(&pool).await {
                Ok(new_tickers) if new_tickers != current_tickers => {
                    info!(
                        old_count = current_tickers.len(),
                        new_count = new_tickers.len(),
                        "ticker list changed — reconnecting stream"
                    );
                    current_tickers = new_tickers;
                    stream_opt = None; // force reconnect with new ticker set
                }
                Ok(_) => {} // unchanged
                Err(e) => warn!(error = %e, "failed to refresh ticker list"),
            }
        }

        // Resolve Uics for current tickers if we don't have a stream.
        if stream_opt.is_none() && !current_tickers.is_empty() {
            healthy.store(false, Ordering::Relaxed);

            let mut resolved = Vec::new();
            for ticker in &current_tickers {
                match uic_resolver
                    .resolve_with_cfd_check(ticker, &token.access_token)
                    .await
                {
                    Ok(uic) => {
                        info!(
                            ticker,
                            uic = uic.uic,
                            exchange_id = %uic.exchange_id,
                            currency = %uic.currency,
                            "resolved Uic"
                        );
                        resolved.push(uic);
                    }
                    Err(e) => {
                        error!(ticker, error = %e, "Uic resolution failed — skipping ticker");
                    }
                }
            }

            if resolved.is_empty() {
                warn!("no Uics resolved — will retry on next refresh");
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }

            match SaxoBarStream::connect(config.clone(), token.clone(), resolved, http.clone())
                .await
            {
                Ok(s) => {
                    info!("Saxo WebSocket stream connected");
                    healthy.store(true, Ordering::Relaxed);
                    stream_opt = Some(s);
                }
                Err(e) => {
                    error!(error = %e, "failed to connect Saxo stream — will retry");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            }
        }

        // Drain bars from the stream.
        if let Some(stream) = &mut stream_opt {
            match tokio::time::timeout(Duration::from_secs(5), stream.receiver.recv()).await {
                Ok(Some(bar)) => {
                    info!(
                        ticker = %bar.asset.ticker,
                        open = bar.open,
                        high = bar.high,
                        low = bar.low,
                        close = bar.close,
                        volume = bar.volume,
                        currency = %bar.currency,
                        "bar completed — publishing to Kafka"
                    );
                    if let Err(e) = producer.publish_bar(&args.kafka_topic, &bar).await {
                        error!(error = %e, "failed to publish bar to Kafka");
                    }
                }
                Ok(None) => {
                    // Channel closed — stream ended.
                    warn!("Saxo bar stream ended — will reconnect");
                    healthy.store(false, Ordering::Relaxed);
                    stream_opt = None;
                }
                Err(_) => {
                    // Timeout — no bar ready yet, continue loop.
                }
            }
        } else {
            // No tickers yet — idle poll.
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    info!("saxo_stream service shut down cleanly");
    Ok(())
}
