/// Persistent Saxo Bank WebSocket streaming ingestion service.
///
/// The only non-CronJob binary in `chronicle` — deployed as a Kubernetes
/// Deployment (single replica) since a WebSocket stream needs a
/// long-running process.
///
/// `GET /health` on `HEALTH_PORT` is served for the entire process
/// lifetime and reflects two live criteria: Postgres reachability, and
/// whether the persisted Saxo refresh token has actually expired (a token
/// that has never been rotated yet — first boot, or recovering from a
/// rejected stored token, NEX-107 — reads as healthy, not failing).
/// `POST /tokens` (used by `nexus saxo auth`) is a route on this same
/// server. WebSocket connectivity does not gate `/health` — see
/// `saxo_ws_connected` on `/metrics` instead (NEX-106).
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
use metrics::{counter, describe_counter, describe_gauge, gauge};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use sqlx::postgres::PgPoolOptions;
use sqlx::Row;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::{mpsc, watch};
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

    #[arg(
        long,
        env = "SAXO_AUTH_BASE",
        default_value = "https://live.logonvalidation.net"
    )]
    saxo_auth_base: String,

    #[arg(long, env = "SAXO_CLIENT_ID")]
    saxo_client_id: String,

    #[arg(long, env = "SAXO_CLIENT_SECRET")]
    saxo_client_secret: String,

    #[arg(long, env = "TICKER_REFRESH_INTERVAL_SECS", default_value = "300")]
    ticker_refresh_interval_secs: u64,

    #[arg(long, env = "HEALTH_PORT", default_value = "8080")]
    health_port: u16,

    #[arg(long, env = "BAR_WINDOW_SECS", default_value = "60")]
    bar_window_secs: i64,
}

const SAXO_TOKEN_SOURCE: &str = "saxo";

struct PgTokenStore {
    pool: sqlx::PgPool,
}

impl PgTokenStore {
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

// `expires_at_unix == 0` means no confirmed-valid token exists yet (first
// boot, or waiting for re-registration after a rejected one, NEX-107) —
// treated as healthy since restarting can't fix either case and the fix is
// already in progress.
fn refresh_token_healthy(token_expires_at_unix: &AtomicI64, now_unix: i64) -> bool {
    let expires_at = token_expires_at_unix.load(Ordering::Relaxed);
    expires_at == 0 || expires_at > now_unix
}

async fn db_reachable(pool: &sqlx::PgPool) -> bool {
    tokio::time::timeout(
        Duration::from_secs(2),
        sqlx::query("SELECT 1").execute(pool),
    )
    .await
    .map(|result| result.is_ok())
    .unwrap_or(false)
}

fn combine_health(db_ok: bool, token_ok: bool) -> (&'static str, String) {
    let mut problems = Vec::new();
    if !db_ok {
        problems.push("database unavailable");
    }
    if !token_ok {
        problems.push("refresh token expired");
    }
    if problems.is_empty() {
        ("200 OK", "ok".to_string())
    } else {
        ("503 Service Unavailable", problems.join(", "))
    }
}

/// Flat JSON body accepted by `POST /tokens`, sent by `nexus saxo auth`.
#[derive(serde::Deserialize)]
struct TokenRegistrationBody {
    access_token: String,
    refresh_token: String,
    access_token_expires_at: chrono::DateTime<chrono::Utc>,
    refresh_token_expires_at: chrono::DateTime<chrono::Utc>,
}

impl From<TokenRegistrationBody> for RotatedToken {
    fn from(b: TokenRegistrationBody) -> Self {
        RotatedToken {
            access_token: SaxoToken {
                access_token: b.access_token,
                expires_at: b.access_token_expires_at,
            },
            refresh_token: b.refresh_token,
            refresh_token_expires_at: b.refresh_token_expires_at,
        }
    }
}

enum RegistrationOutcome {
    Registered(RotatedToken),
    Rejected,
}

// Called after `handle_connection` has already consumed the request line.
async fn read_registration_body(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    write_half: &mut OwnedWriteHalf,
) -> RegistrationOutcome {
    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.is_err() {
            let _ = write_half
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                .await;
            return RegistrationOutcome::Rejected;
        }
        if line.trim().is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            content_length = rest.trim().parse().unwrap_or(0);
        }
    }

    if content_length == 0 {
        let _ = write_half
            .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
            .await;
        return RegistrationOutcome::Rejected;
    }

    let mut body = vec![0u8; content_length];
    if reader.read_exact(&mut body).await.is_err() {
        let _ = write_half
            .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
            .await;
        return RegistrationOutcome::Rejected;
    }

    match serde_json::from_slice::<TokenRegistrationBody>(&body) {
        Ok(reg) if !reg.access_token.is_empty() && !reg.refresh_token.is_empty() => {
            let _ = write_half
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await;
            RegistrationOutcome::Registered(reg.into())
        }
        _ => {
            let _ = write_half
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                .await;
            RegistrationOutcome::Rejected
        }
    }
}

async fn handle_connection(
    stream: tokio::net::TcpStream,
    pool: sqlx::PgPool,
    token_expires_at_unix: Arc<AtomicI64>,
    prometheus_handle: PrometheusHandle,
    registration_tx: mpsc::Sender<RotatedToken>,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await.is_err() {
        return;
    }
    let request_line = request_line.trim_end().to_string();

    if request_line.starts_with("GET /metrics") {
        let now_unix = Utc::now().timestamp();
        let expires_at = token_expires_at_unix.load(Ordering::Relaxed);
        let seconds_remaining: f64 = if expires_at > 0 {
            (expires_at - now_unix).max(0) as f64
        } else {
            -1.0
        };
        gauge!("saxo_refresh_token_seconds_remaining").set(seconds_remaining);
        let body = prometheus_handle.render();
        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Length: {}\r\n\
             Content-Type: text/plain; version=0.0.4\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = write_half.write_all(response.as_bytes()).await;
        return;
    }

    if request_line.starts_with("POST /tokens") {
        if let RegistrationOutcome::Registered(rotated) =
            read_registration_body(&mut reader, &mut write_half).await
        {
            info!("Saxo tokens received via POST /tokens");
            // No-op if nobody's currently waiting (channel full or closed).
            let _ = registration_tx.try_send(rotated);
        }
        return;
    }

    if request_line.contains(" /tokens ") {
        let _ = write_half
            .write_all(
                b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nAllow: POST\r\n\r\n",
            )
            .await;
        return;
    }

    let db_ok = db_reachable(&pool).await;
    let token_ok = refresh_token_healthy(&token_expires_at_unix, Utc::now().timestamp());
    let (status, body) = combine_health(db_ok, token_ok);
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n{body}",
        body.len()
    );
    let _ = write_half.write_all(response.as_bytes()).await;
}

async fn run_http_server(
    listener: TcpListener,
    pool: sqlx::PgPool,
    token_expires_at_unix: Arc<AtomicI64>,
    prometheus_handle: PrometheusHandle,
    registration_tx: mpsc::Sender<RotatedToken>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(handle_connection(
                    stream,
                    pool.clone(),
                    token_expires_at_unix.clone(),
                    prometheus_handle.clone(),
                    registration_tx.clone(),
                ));
            }
            Err(e) => {
                warn!(error = %e, "health endpoint accept error");
            }
        }
    }
}

async fn wait_for_registration(
    registration_rx: &mut mpsc::Receiver<RotatedToken>,
) -> anyhow::Result<RotatedToken> {
    let mut heartbeat = interval(Duration::from_secs(60));
    heartbeat.tick().await;

    loop {
        tokio::select! {
            token = registration_rx.recv() => {
                return token.context("registration channel closed unexpectedly");
            }
            _ = heartbeat.tick() => {
                info!("waiting for Saxo token registration via POST /tokens");
                continue;
            }
        }
    }
}

// Shared by both the fresh-bootstrap and NEX-107 recovery paths. Does NOT
// call `SaxoAuth::refresh()` on the token it receives — it's already valid,
// and Saxo invalidates the old refresh token the instant it's used.
#[allow(clippy::too_many_arguments)]
async fn bootstrap_from_registration(
    registration_rx: &mut mpsc::Receiver<RotatedToken>,
    http: reqwest::Client,
    saxo_auth_base: &str,
    saxo_client_id: &str,
    saxo_client_secret: &str,
    pg_store: &PgTokenStore,
    pool: sqlx::PgPool,
    token_expires_at_unix: &Arc<AtomicI64>,
) -> anyhow::Result<(SaxoToken, SaxoAuth)> {
    let rotated = wait_for_registration(registration_rx).await?;

    pg_store.save(&rotated).await;
    token_expires_at_unix.store(
        rotated.refresh_token_expires_at.timestamp(),
        Ordering::Relaxed,
    );

    let initial = rotated.access_token.clone();
    let token_store: Arc<dyn TokenStore> = Arc::new(PgTokenStore { pool });
    let auth = SaxoAuth::new(
        http,
        format!("{saxo_auth_base}/token"),
        saxo_client_id,
        saxo_client_secret,
        rotated.refresh_token,
        token_store,
    );

    Ok((initial, auth))
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

    let prometheus_handle = PrometheusBuilder::new()
        .install_recorder()
        .context("failed to install Prometheus recorder")?;

    describe_gauge!(
        "saxo_refresh_token_seconds_remaining",
        "Seconds until the Saxo refresh token expires. \
         -1 means no successful rotation has occurred yet."
    );
    describe_counter!(
        "saxo_refresh_token_failures_total",
        "Total number of Saxo token refresh failures since startup."
    );
    describe_gauge!(
        "saxo_ws_connected",
        "Whether the Saxo WebSocket bar stream is currently connected \
         (0 also when no FNSE tickers are registered — informational only, \
         does not affect /health)."
    );

    let token_expires_at_unix = Arc::new(AtomicI64::new(0));

    // Bound before anything else that could fail or block — /health is
    // servable from here on for the rest of the process lifetime.
    let health_listener = TcpListener::bind(format!("0.0.0.0:{}", args.health_port))
        .await
        .with_context(|| {
            format!(
                "failed to bind health/metrics/tokens endpoint on port {}",
                args.health_port
            )
        })?;
    info!(
        port = args.health_port,
        "health/metrics/tokens endpoint listening"
    );

    let (registration_tx, mut registration_rx) = mpsc::channel::<RotatedToken>(1);
    tokio::spawn(run_http_server(
        health_listener,
        pool.clone(),
        token_expires_at_unix.clone(),
        prometheus_handle.clone(),
        registration_tx,
    ));

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

    let pg_store = PgTokenStore { pool: pool.clone() };

    let stored = match pg_store.load_refresh_token().await {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "failed to read oauth_tokens — treating as empty (bootstrap mode)");
            None
        }
    };

    let (initial_token, mut saxo_auth): (SaxoToken, SaxoAuth) = if let Some(refresh_token) = stored
    {
        info!(
            "loaded refresh token from oauth_tokens — calling refresh() for initial access token"
        );
        let token_store: Arc<dyn TokenStore> = Arc::new(PgTokenStore { pool: pool.clone() });
        let mut auth = SaxoAuth::new(
            http.clone(),
            format!("{}/token", args.saxo_auth_base),
            args.saxo_client_id.clone(),
            args.saxo_client_secret.clone(),
            refresh_token,
            token_store,
        );
        match auth.refresh().await {
            Ok(rotated) => {
                token_expires_at_unix.store(
                    rotated.refresh_token_expires_at.timestamp(),
                    Ordering::Relaxed,
                );
                (rotated.access_token, auth)
            }
            Err(e) => {
                // Restarting can't fix an invalid external credential, so
                // exiting here would only crash-loop forever (NEX-107).
                error!(
                    error = %e,
                    "initial refresh of stored Saxo token failed — waiting for \
                     re-registration via `nexus saxo auth` instead of exiting"
                );
                token_expires_at_unix.store(0, Ordering::Relaxed);
                bootstrap_from_registration(
                    &mut registration_rx,
                    http.clone(),
                    &args.saxo_auth_base,
                    &args.saxo_client_id,
                    &args.saxo_client_secret,
                    &pg_store,
                    pool.clone(),
                    &token_expires_at_unix,
                )
                .await?
            }
        }
    } else {
        bootstrap_from_registration(
            &mut registration_rx,
            http.clone(),
            &args.saxo_auth_base,
            &args.saxo_client_id,
            &args.saxo_client_secret,
            &pg_store,
            pool.clone(),
            &token_expires_at_unix,
        )
        .await?
    };

    drop(registration_rx);

    let shared_token: SharedToken = Arc::new(Mutex::new(initial_token));

    {
        let shared_token = shared_token.clone();
        let streaming_base = args.saxo_streaming_base.clone();
        let context_id = config.context_id.clone();
        let threshold_secs = config.token_refresh_threshold_secs;
        let token_expires_at_unix = token_expires_at_unix.clone();
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
                        // Record the new expiry regardless of whether
                        // WebSocket reauth below succeeds — it drives both
                        // /health and the seconds-remaining metric.
                        token_expires_at_unix.store(
                            rotated.refresh_token_expires_at.timestamp(),
                            Ordering::Relaxed,
                        );

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
                        counter!("saxo_refresh_token_failures_total").increment(1);
                        error!(error = %e, "Saxo token refresh failed — will retry next tick");
                    }
                }
            }
        });
    }

    let mut refresh_ticker = interval(Duration::from_secs(args.ticker_refresh_interval_secs));
    refresh_ticker.tick().await;

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
            gauge!("saxo_ws_connected").set(0.0_f64);

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
                    gauge!("saxo_ws_connected").set(1.0_f64);
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
                    gauge!("saxo_ws_connected").set(0.0_f64);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpStream;

    fn token_expiry(expires_at_unix: i64) -> AtomicI64 {
        AtomicI64::new(expires_at_unix)
    }

    #[test]
    fn health_is_ok_with_valid_unexpired_token_even_with_no_ws_connection() {
        let expiry = token_expiry(2_000_000_000);
        assert!(refresh_token_healthy(&expiry, 1_000_000_000));
    }

    #[test]
    fn health_is_ok_in_brief_zero_sentinel_startup_window() {
        let expiry = token_expiry(0);
        assert!(refresh_token_healthy(&expiry, 1_000_000_000));
    }

    #[test]
    fn health_is_unhealthy_once_refresh_token_has_actually_expired() {
        let expiry = token_expiry(1_000_000_000);
        assert!(!refresh_token_healthy(&expiry, 1_000_000_001));
    }

    #[test]
    fn health_is_ok_exactly_at_expiry_boundary_minus_one() {
        let expiry = token_expiry(1_000_000_000);
        assert!(refresh_token_healthy(&expiry, 999_999_999));
    }

    #[test]
    fn combine_health_is_200_ok_when_all_criteria_pass() {
        let (status, body) = combine_health(true, true);
        assert_eq!(status, "200 OK");
        assert_eq!(body, "ok");
    }

    #[test]
    fn combine_health_is_503_when_db_unreachable() {
        let (status, body) = combine_health(false, true);
        assert_eq!(status, "503 Service Unavailable");
        assert!(body.contains("database unavailable"));
        assert!(!body.contains("refresh token expired"));
    }

    #[test]
    fn combine_health_is_503_when_token_expired() {
        let (status, body) = combine_health(true, false);
        assert_eq!(status, "503 Service Unavailable");
        assert!(body.contains("refresh token expired"));
        assert!(!body.contains("database unavailable"));
    }

    #[test]
    fn combine_health_reports_both_problems_when_both_fail() {
        let (status, body) = combine_health(false, false);
        assert_eq!(status, "503 Service Unavailable");
        assert!(body.contains("database unavailable"));
        assert!(body.contains("refresh token expired"));
    }

    #[tokio::test]
    async fn db_reachable_is_false_for_unreachable_database() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://user:pass@127.0.0.1:1/nonexistent")
            .expect("connect_lazy should not eagerly connect");
        assert!(!db_reachable(&pool).await);
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL"]
    async fn db_reachable_is_true_for_a_live_database() {
        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for this test");
        let pool = sqlx::PgPool::connect(&database_url)
            .await
            .expect("failed to connect to test Postgres");
        assert!(db_reachable(&pool).await);
    }

    fn dummy_pool() -> sqlx::PgPool {
        PgPoolOptions::new()
            .connect_lazy("postgres://user:pass@127.0.0.1:1/nonexistent")
            .expect("connect_lazy should not eagerly connect")
    }

    fn dummy_prometheus_handle() -> PrometheusHandle {
        PrometheusBuilder::new().build_recorder().handle()
    }

    const VALID_ACCESS_TOKEN: &str = "acc_test_123";
    const VALID_REFRESH_TOKEN: &str = "ref_test_456";

    fn valid_body() -> String {
        format!(
            r#"{{"access_token":"{VALID_ACCESS_TOKEN}","refresh_token":"{VALID_REFRESH_TOKEN}","access_token_expires_at":"2099-01-01T00:00:00Z","refresh_token_expires_at":"2099-01-01T01:00:00Z"}}"#
        )
    }

    fn post_request(body: &str) -> Vec<u8> {
        format!(
            "POST /tokens HTTP/1.1\r\n\
             Host: localhost\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {len}\r\n\
             \r\n\
             {body}",
            len = body.len()
        )
        .into_bytes()
    }

    fn headers_and_body(body: &str) -> Vec<u8> {
        format!(
            "Host: localhost\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {len}\r\n\
             \r\n\
             {body}",
            len = body.len()
        )
        .into_bytes()
    }

    async fn registration_body_roundtrip(headers_and_body: &[u8]) -> (RegistrationOutcome, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let req = headers_and_body.to_vec();

        tokio::join!(
            async {
                let (stream, _) = listener.accept().await.unwrap();
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                read_registration_body(&mut reader, &mut write_half).await
            },
            async {
                let mut client = TcpStream::connect(addr).await.unwrap();
                client.write_all(&req).await.unwrap();
                let mut buf = Vec::new();
                client.read_to_end(&mut buf).await.unwrap();
                String::from_utf8_lossy(&buf).to_string()
            }
        )
    }

    #[tokio::test]
    async fn read_registration_body_valid_post_returns_registered() {
        let body = valid_body();
        let (outcome, response) = registration_body_roundtrip(&headers_and_body(&body)).await;

        match outcome {
            RegistrationOutcome::Registered(token) => {
                assert_eq!(token.access_token.access_token, VALID_ACCESS_TOKEN);
                assert_eq!(token.refresh_token, VALID_REFRESH_TOKEN);
            }
            RegistrationOutcome::Rejected => panic!("expected Registered, got Rejected"),
        }
        assert!(response.starts_with("HTTP/1.1 200"));
    }

    #[tokio::test]
    async fn read_registration_body_malformed_json_returns_rejected() {
        let (outcome, response) =
            registration_body_roundtrip(&headers_and_body("this is not json")).await;
        assert!(matches!(outcome, RegistrationOutcome::Rejected));
        assert!(response.contains("400"));
    }

    #[tokio::test]
    async fn read_registration_body_missing_access_token_returns_rejected() {
        let body = format!(
            r#"{{"refresh_token":"{VALID_REFRESH_TOKEN}","access_token_expires_at":"2099-01-01T00:00:00Z","refresh_token_expires_at":"2099-01-01T01:00:00Z"}}"#
        );
        let (outcome, response) = registration_body_roundtrip(&headers_and_body(&body)).await;
        assert!(matches!(outcome, RegistrationOutcome::Rejected));
        assert!(response.contains("400"));
    }

    #[tokio::test]
    async fn read_registration_body_empty_access_token_returns_rejected() {
        let body = format!(
            r#"{{"access_token":"","refresh_token":"{VALID_REFRESH_TOKEN}","access_token_expires_at":"2099-01-01T00:00:00Z","refresh_token_expires_at":"2099-01-01T01:00:00Z"}}"#
        );
        let (outcome, response) = registration_body_roundtrip(&headers_and_body(&body)).await;
        assert!(matches!(outcome, RegistrationOutcome::Rejected));
        assert!(response.contains("400"));
    }

    #[tokio::test]
    async fn read_registration_body_missing_refresh_token_returns_rejected() {
        let body = format!(
            r#"{{"access_token":"{VALID_ACCESS_TOKEN}","access_token_expires_at":"2099-01-01T00:00:00Z","refresh_token_expires_at":"2099-01-01T01:00:00Z"}}"#
        );
        let (outcome, response) = registration_body_roundtrip(&headers_and_body(&body)).await;
        assert!(matches!(outcome, RegistrationOutcome::Rejected));
        assert!(response.contains("400"));
    }

    async fn connection_roundtrip(request: &[u8]) -> (String, mpsc::Receiver<RotatedToken>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let req = request.to_vec();
        let (tx, rx) = mpsc::channel::<RotatedToken>(1);
        let pool = dummy_pool();
        let prometheus_handle = dummy_prometheus_handle();
        let token_expires_at_unix = Arc::new(AtomicI64::new(0));

        let (_, response) = tokio::join!(
            async {
                let (stream, _) = listener.accept().await.unwrap();
                handle_connection(stream, pool, token_expires_at_unix, prometheus_handle, tx).await;
            },
            async {
                let mut client = TcpStream::connect(addr).await.unwrap();
                client.write_all(&req).await.unwrap();
                let mut buf = Vec::new();
                client.read_to_end(&mut buf).await.unwrap();
                String::from_utf8_lossy(&buf).to_string()
            }
        );

        (response, rx)
    }

    #[tokio::test]
    async fn handle_connection_get_health_reports_db_unavailable() {
        let request = b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let (response, _rx) = connection_roundtrip(request).await;
        assert!(response.starts_with("HTTP/1.1 503"));
        assert!(response.contains("database unavailable"));
    }

    #[tokio::test]
    async fn handle_connection_get_metrics_returns_200() {
        let request = b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let (response, _rx) = connection_roundtrip(request).await;
        assert!(response.starts_with("HTTP/1.1 200"));
    }

    #[tokio::test]
    async fn handle_connection_get_tokens_returns_405() {
        let request = b"GET /tokens HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let (response, _rx) = connection_roundtrip(request).await;
        assert!(response.contains("405"));
    }

    #[tokio::test]
    async fn handle_connection_post_tokens_delivers_to_channel() {
        let body = valid_body();
        let (response, mut rx) = connection_roundtrip(&post_request(&body)).await;
        assert!(response.starts_with("HTTP/1.1 200"));
        let received = rx
            .try_recv()
            .expect("registration should reach the channel");
        assert_eq!(received.access_token.access_token, VALID_ACCESS_TOKEN);
        assert_eq!(received.refresh_token, VALID_REFRESH_TOKEN);
    }

    #[tokio::test]
    async fn wait_for_registration_blocks_then_returns_delivered_token() {
        let (tx, mut rx) = mpsc::channel::<RotatedToken>(1);
        let task = tokio::spawn(async move { wait_for_registration(&mut rx).await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!task.is_finished());

        tx.send(RotatedToken {
            access_token: SaxoToken {
                access_token: VALID_ACCESS_TOKEN.to_string(),
                expires_at: chrono::Utc::now() + chrono::Duration::seconds(1200),
            },
            refresh_token: VALID_REFRESH_TOKEN.to_string(),
            refresh_token_expires_at: chrono::Utc::now() + chrono::Duration::seconds(3600),
        })
        .await
        .unwrap();

        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("timed out")
            .unwrap()
            .expect("wait_for_registration should succeed");
        assert_eq!(result.access_token.access_token, VALID_ACCESS_TOKEN);
        assert_eq!(result.refresh_token, VALID_REFRESH_TOKEN);
    }

    #[tokio::test]
    async fn wait_for_registration_errors_if_channel_closed() {
        let (tx, mut rx) = mpsc::channel::<RotatedToken>(1);
        drop(tx);
        assert!(wait_for_registration(&mut rx).await.is_err());
    }

    // Requires a real Postgres with a pre-seeded `oauth_tokens` row:
    //   DATABASE_URL=postgres://... cargo test -p chronicle --bin saxo_stream \
    //     -- --ignored restart_path_calls_refresh_exactly_once
    #[tokio::test]
    #[ignore = "requires DATABASE_URL with a pre-seeded oauth_tokens row"]
    async fn restart_path_calls_refresh_exactly_once() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for this test");
        let pool = sqlx::PgPool::connect(&database_url)
            .await
            .expect("failed to connect to test Postgres");

        sqlx::query(
            "INSERT INTO oauth_tokens (source, refresh_token, refresh_token_expires_at, updated_at)
             VALUES ($1, $2, NOW() + INTERVAL '1 hour', NOW())
             ON CONFLICT (source) DO UPDATE SET
                 refresh_token = EXCLUDED.refresh_token,
                 refresh_token_expires_at = EXCLUDED.refresh_token_expires_at,
                 updated_at = NOW()",
        )
        .bind(SAXO_TOKEN_SOURCE)
        .bind("test_refresh_token_for_restart_path")
        .execute(&pool)
        .await
        .expect("failed to seed oauth_tokens");

        let saxo_mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"access_token":"new_acc","refresh_token":"new_ref","expires_in":1200,"refresh_token_expires_in":3600}"#,
            ))
            .expect(1)
            .mount(&saxo_mock)
            .await;

        let store = PgTokenStore { pool: pool.clone() };
        let stored = store
            .load_refresh_token()
            .await
            .expect("load_refresh_token failed");
        assert!(stored.is_some());

        let token_store: Arc<dyn TokenStore> = Arc::new(PgTokenStore { pool });
        let http = reqwest::Client::new();
        let token_url = format!("{}/token", saxo_mock.uri());
        let mut auth = SaxoAuth::new(
            http,
            token_url,
            "client_id",
            "client_secret",
            stored.unwrap(),
            token_store,
        );

        let rotated = auth.refresh().await.expect("refresh() should succeed");
        assert_eq!(rotated.access_token.access_token, "new_acc");
    }

    // Requires a real Postgres for the persistence assertion:
    //   DATABASE_URL=postgres://... cargo test -p chronicle --bin saxo_stream \
    //     -- --ignored bootstrap_from_registration_recovers_and_persists_new_token
    #[tokio::test]
    #[ignore = "requires DATABASE_URL"]
    async fn bootstrap_from_registration_recovers_and_persists_new_token() {
        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for this test");
        let pool = sqlx::PgPool::connect(&database_url)
            .await
            .expect("failed to connect to test Postgres");

        let pg_store = PgTokenStore { pool: pool.clone() };
        let token_expires_at_unix = Arc::new(AtomicI64::new(0));
        let (tx, mut rx) = mpsc::channel::<RotatedToken>(1);

        tx.send(RotatedToken {
            access_token: SaxoToken {
                access_token: VALID_ACCESS_TOKEN.to_string(),
                expires_at: chrono::Utc::now() + chrono::Duration::seconds(1200),
            },
            refresh_token: VALID_REFRESH_TOKEN.to_string(),
            refresh_token_expires_at: chrono::Utc::now() + chrono::Duration::seconds(3600),
        })
        .await
        .unwrap();

        let (initial, _auth) = tokio::time::timeout(
            Duration::from_secs(2),
            bootstrap_from_registration(
                &mut rx,
                reqwest::Client::new(),
                "http://unused.invalid",
                "client_id",
                "client_secret",
                &pg_store,
                pool.clone(),
                &token_expires_at_unix,
            ),
        )
        .await
        .expect("timed out")
        .expect("bootstrap_from_registration should succeed, not exit the process");

        assert_eq!(initial.access_token, VALID_ACCESS_TOKEN);
        assert_ne!(token_expires_at_unix.load(Ordering::Relaxed), 0);

        let persisted = pg_store
            .load_refresh_token()
            .await
            .expect("load_refresh_token failed");
        assert_eq!(persisted.as_deref(), Some(VALID_REFRESH_TOKEN));
    }
}
