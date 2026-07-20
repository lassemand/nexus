/// Persistent Saxo Bank WebSocket streaming ingestion service.
///
/// This is the only non-CronJob binary in `chronicle` — deployed as a
/// Kubernetes Deployment (single replica) because a WebSocket stream requires
/// a long-running process rather than a one-shot job.
///
/// Health: `GET /health` on `HEALTH_PORT` (default 8080) returns 200 when
/// the WebSocket is connected, 503 when it is not — so k8s restarts the pod
/// on a wedged connection, not just a crashed process.
///
/// # Token rotation
///
/// A periodic task (spawned in `main`, independent of the WebSocket stream)
/// owns the only `SaxoAuth` instance and is the sole writer of the shared
/// `SharedToken`. It reauthorizes whatever connection is currently live via
/// `refresh_on_stream()` — a REST call keyed by `context_id`, decoupled from
/// any specific stream object — so the stream's own reconnect logic never
/// needs to know about refresh at all; it just reads the latest token from
/// `SharedToken` on each connect/reconnect. Persistence to `oauth_tokens` is
/// not this binary's main loop's job either — it happens inside
/// `SaxoAuth::refresh()` itself via the `PgTokenStore` handle below.
mod db;
mod kafka;

use alpha::saxo::{
    RotatedToken, SaxoAuth, SaxoBarStream, SaxoConfig, SaxoToken, SharedToken, TokenStore,
    UicResolver,
};
use anyhow::Context;
use chrono::Utc;
use clap::Parser;
use kafka::ChronicleProducer;
use sqlx::postgres::PgPoolOptions;
use sqlx::Row;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::watch;
use tokio::time::{interval, Duration};
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(about = "Persistent Saxo WebSocket streaming ingestion — Deployment, not CronJob")]
struct Args {
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    #[arg(long, env = "KAFKA_BROKERS")]
    kafka_brokers: String,

    #[arg(long, env = "KAFKA_TOPIC", default_value = "market.bars")]
    kafka_topic: String,

    #[arg(
        long,
        env = "SAXO_API_BASE",
        default_value = "https://gateway.saxobank.com/openapi"
    )]
    saxo_api_base: String,

    #[arg(
        long,
        env = "SAXO_STREAMING_BASE",
        default_value = "https://live-streaming.saxobank.com/oapi/streaming/ws"
    )]
    saxo_streaming_base: String,

    /// OAuth2 client ID (from developer.saxo app registration).
    #[arg(long, env = "SAXO_CLIENT_ID")]
    saxo_client_id: String,

    /// OAuth2 client secret.
    #[arg(long, env = "SAXO_CLIENT_SECRET")]
    saxo_client_secret: String,

    /// Bootstrap refresh token — used only if oauth_tokens DB table is empty.
    /// After first rotation the DB value takes precedence on restart.
    #[arg(long, env = "SAXO_REFRESH_TOKEN")]
    saxo_refresh_token: String,

    /// Initial OAuth2 access token (bootstrap only).
    #[arg(long, env = "SAXO_ACCESS_TOKEN")]
    saxo_access_token: String,

    /// Initial access token expiry as Unix timestamp.
    /// Set to a near-future value (e.g. `$(date +%s -d '+10 seconds')`) for
    /// local testing to force a rotation within seconds.
    #[arg(long, env = "SAXO_TOKEN_EXPIRES_AT")]
    saxo_token_expires_at: i64,

    #[arg(long, env = "TICKER_REFRESH_INTERVAL_SECS", default_value = "300")]
    ticker_refresh_interval_secs: u64,

    #[arg(long, env = "HEALTH_PORT", default_value = "8080")]
    health_port: u16,

    #[arg(long, env = "BAR_WINDOW_SECS", default_value = "60")]
    bar_window_secs: i64,
}

/// Identifies which OAuth token an `oauth_tokens` row represents. Only one
/// exists today, but keying by a meaningful value instead of an opaque
/// `id = 1` singleton leaves room to add more later (e.g. a second broker
/// or environment) without another schema redesign.
const SAXO_TOKEN_SOURCE: &str = "saxo";

/// `TokenStore` backed by the `oauth_tokens` table. This is the only place in
/// the binary that knows about Postgres for token persistence — both the
/// bootstrap read in `main` and every write `SaxoAuth::refresh()` triggers
/// (ADR-0003) go through this one type, so there's a single owner of the
/// `oauth_tokens` table's SQL.
struct PgTokenStore {
    pool: sqlx::PgPool,
}

impl PgTokenStore {
    /// Read the latest refresh token from `oauth_tokens`.
    /// Returns `None` if the row doesn't exist yet (bootstrap state).
    async fn load_refresh_token(&self) -> anyhow::Result<Option<String>> {
        let row = sqlx::query("SELECT refresh_token FROM oauth_tokens WHERE source = $1")
            .bind(SAXO_TOKEN_SOURCE)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get("refresh_token")))
    }
}

#[async_trait::async_trait]
impl TokenStore for PgTokenStore {
    async fn save(&self, rotated: &RotatedToken) {
        let result = sqlx::query(
            r#"
            INSERT INTO oauth_tokens (source, refresh_token, refresh_token_expires_at, updated_at)
            VALUES ($1, $2, $3, NOW())
            ON CONFLICT (source) DO UPDATE SET
                refresh_token             = EXCLUDED.refresh_token,
                refresh_token_expires_at  = EXCLUDED.refresh_token_expires_at,
                updated_at                = NOW()
            "#,
        )
        .bind(SAXO_TOKEN_SOURCE)
        .bind(&rotated.refresh_token)
        .bind(rotated.refresh_token_expires_at)
        .execute(&self.pool)
        .await;

        if let Err(e) = result {
            error!(error = %e, "failed to persist rotated refresh token to oauth_tokens");
        }
    }
}

async fn load_fnse_tickers(pool: &sqlx::PgPool) -> anyhow::Result<Vec<String>> {
    let rows =
        sqlx::query("SELECT ticker FROM companies WHERE exchange_mic = 'FNSE' ORDER BY ticker")
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|r| r.get("ticker")).collect())
}

/// Shared observability state updated by the main loop.
struct Metrics {
    /// WebSocket connection health (for /health endpoint).
    ws_healthy: AtomicBool,
    /// Unix timestamp when the current refresh token expires.
    /// 0 = no successful rotation yet (bootstrap/grace state).
    refresh_token_expires_at_unix: AtomicI64,
    /// Total number of token refresh failures since startup.
    refresh_failures_total: AtomicU64,
}

async fn serve_health(port: u16, metrics: Arc<Metrics>) {
    let listener = match TcpListener::bind(format!("0.0.0.0:{port}")).await {
        Ok(l) => l,
        Err(e) => {
            error!(port, error = %e, "failed to bind health/metrics endpoint");
            return;
        }
    };
    info!(port, "health/metrics endpoint listening");

    loop {
        match listener.accept().await {
            Err(e) => {
                warn!(error = %e, "health endpoint accept error");
                continue;
            }
            Ok((mut stream, _)) => {
                // Read the first line to distinguish /health from /metrics.
                let mut buf = [0u8; 256];
                let n = match tokio::time::timeout(Duration::from_millis(100), stream.readable())
                    .await
                {
                    Ok(Ok(())) => stream.try_read(&mut buf).unwrap_or(0),
                    _ => 0,
                };
                let req = std::str::from_utf8(&buf[..n]).unwrap_or("");
                let is_metrics = req.starts_with("GET /metrics");

                let response = if is_metrics {
                    let now_unix = Utc::now().timestamp();
                    let expires_at = metrics
                        .refresh_token_expires_at_unix
                        .load(Ordering::Relaxed);
                    let seconds_remaining = if expires_at > 0 {
                        (expires_at - now_unix).max(0)
                    } else {
                        -1 // -1 = no rotation yet (bootstrap)
                    };
                    let failures = metrics.refresh_failures_total.load(Ordering::Relaxed);

                    let body = format!(
                        "# HELP saxo_refresh_token_seconds_remaining \
                         Seconds until the Saxo refresh token expires. \
                         -1 means no successful rotation has occurred yet.\n\
                         # TYPE saxo_refresh_token_seconds_remaining gauge\n\
                         saxo_refresh_token_seconds_remaining {seconds_remaining}\n\
                         # HELP saxo_refresh_token_failures_total \
                         Total number of Saxo token refresh failures since startup.\n\
                         # TYPE saxo_refresh_token_failures_total counter\n\
                         saxo_refresh_token_failures_total {failures}\n"
                    );
                    format!(
                        "HTTP/1.1 200 OK\r\n\
                         Content-Length: {}\r\n\
                         Content-Type: text/plain; version=0.0.4\r\n\r\n{}",
                        body.len(),
                        body
                    )
                } else {
                    let ok = metrics.ws_healthy.load(Ordering::Relaxed);
                    let (status, body) = if ok {
                        ("200 OK", "ok")
                    } else {
                        ("503 Service Unavailable", "unhealthy")
                    };
                    format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n{body}",
                        body.len()
                    )
                };
                let _ = stream.write_all(response.as_bytes()).await;
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/.env")).ok();
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    let pool = PgPoolOptions::new()
        .max_connections(3)
        .connect(&args.database_url)
        .await
        .context("failed to connect to postgres")?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("migrations failed")?;

    let producer =
        ChronicleProducer::new(&args.kafka_brokers).context("failed to create Kafka producer")?;

    let metrics = Arc::new(Metrics {
        ws_healthy: AtomicBool::new(false),
        refresh_token_expires_at_unix: AtomicI64::new(0),
        refresh_failures_total: AtomicU64::new(0),
    });
    tokio::spawn(serve_health(args.health_port, metrics.clone()));

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let shutdown_tx = Arc::new(shutdown_tx);
    {
        let tx = shutdown_tx.clone();
        tokio::spawn(async move {
            signal::ctrl_c().await.ok();
            info!("SIGTERM/SIGINT received — shutting down");
            let _ = tx.send(true);
        });
    }

    let tickers = load_fnse_tickers(&pool).await.unwrap_or_default();
    if tickers.is_empty() {
        warn!(
            "no FNSE tickers registered — register via: nexus register GOMX.ST\n\
             will poll every {} seconds",
            args.ticker_refresh_interval_secs
        );
    } else {
        info!(count = tickers.len(), tickers = ?tickers, "loaded FNSE tickers");
    }

    let http = reqwest::Client::builder()
        .user_agent("nexus lasse.alm@gsfleet.io")
        .build()
        .context("failed to build HTTP client")?;

    let uic_resolver = UicResolver::new(http.clone(), &args.saxo_api_base);

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

    let pg_token_store = PgTokenStore { pool: pool.clone() };

    let bootstrap_refresh_token = match pg_token_store.load_refresh_token().await {
        Ok(Some(t)) => {
            info!("loaded refresh token from oauth_tokens table");
            t
        }
        Ok(None) => {
            info!("oauth_tokens table empty — using bootstrap SAXO_REFRESH_TOKEN env var");
            args.saxo_refresh_token.clone()
        }
        Err(e) => {
            warn!(error = %e, "failed to read oauth_tokens — using bootstrap env var");
            args.saxo_refresh_token.clone()
        }
    };

    let token_store: Arc<dyn TokenStore> = Arc::new(pg_token_store);

    let mut saxo_auth = SaxoAuth::new(
        http.clone(),
        format!("{}/token", "https://live.logonvalidation.net"),
        args.saxo_client_id.clone(),
        args.saxo_client_secret.clone(),
        bootstrap_refresh_token,
        token_store,
    );

    let shared_token: SharedToken = Arc::new(Mutex::new(token));

    {
        let shared_token = shared_token.clone();
        let streaming_base = args.saxo_streaming_base.clone();
        let context_id = config.context_id.clone();
        let threshold_secs = config.token_refresh_threshold_secs;
        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(30));
            loop {
                ticker.tick().await;

                let needs_refresh = shared_token
                    .lock()
                    .unwrap()
                    .expires_within_secs(threshold_secs);
                if !needs_refresh {
                    continue;
                }

                match saxo_auth.refresh().await {
                    Ok(rotated) => {
                        if let Err(e) = saxo_auth
                            .refresh_on_stream(
                                &streaming_base,
                                &context_id,
                                &rotated.access_token.access_token,
                            )
                            .await
                        {
                            error!(error = %e, "failed to reauthorize WebSocket after token rotation");
                            continue;
                        }
                        *shared_token.lock().unwrap() = rotated.access_token;
                        info!("Saxo access token rotated and WebSocket reauthorized");
                    }
                    Err(e) => {
                        error!(error = %e, "Saxo token refresh failed — will retry next tick");
                    }
                }
            }
        });
    }

    let mut refresh_ticker = interval(Duration::from_secs(args.ticker_refresh_interval_secs));
    refresh_ticker.tick().await; // consume the immediate first tick

    info!("starting Saxo stream ingestion loop");
    let mut current_tickers = tickers;
    let mut stream_opt: Option<SaxoBarStream> = None;

    loop {
        if *shutdown_rx.borrow() {
            info!("shutdown — exiting");
            break;
        }

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
                    stream_opt = None;
                }
                Ok(_) => {}
                Err(e) => warn!(error = %e, "failed to refresh ticker list"),
            }
        }

        if stream_opt.is_none() && !current_tickers.is_empty() {
            metrics.ws_healthy.store(false, Ordering::Relaxed);

            let access_token_snapshot = shared_token.lock().unwrap().access_token.clone();

            let mut resolved = Vec::new();
            for ticker in &current_tickers {
                match uic_resolver
                    .resolve_with_cfd_check(ticker, &access_token_snapshot)
                    .await
                {
                    Ok(uic) => {
                        info!(ticker, uic = uic.uic, "resolved Uic");
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

            match SaxoBarStream::connect(
                config.clone(),
                shared_token.clone(),
                resolved,
                http.clone(),
            )
            .await
            {
                Ok(s) => {
                    info!("Saxo WebSocket stream connected");
                    metrics.ws_healthy.store(true, Ordering::Relaxed);
                    stream_opt = Some(s);
                }
                Err(e) => {
                    error!(error = %e, "failed to connect Saxo stream — will retry");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            }
        }

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
                        "bar completed"
                    );
                    if let Err(e) = producer.publish_bar(&args.kafka_topic, &bar).await {
                        error!(error = %e, "failed to publish bar to Kafka");
                    }
                }
                Ok(None) => {
                    warn!("Saxo bar stream ended — will reconnect");
                    metrics.ws_healthy.store(false, Ordering::Relaxed);
                    stream_opt = None;
                }
                Err(_) => {}
            }
        } else {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    info!("saxo_stream shut down cleanly");
    Ok(())
}
