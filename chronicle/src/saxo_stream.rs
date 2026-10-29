/// Saxo Bank service: keeps the OAuth token alive and syncs official daily
/// bars for the registered Nordic instruments (NEX-162).
///
/// A Deployment, not a CronJob: the refresh token rotates on every use and
/// lives about an hour, so a long-running process keeps it valid; the daily-bar
/// sweep runs at startup and then every day at 18:30 Stockholm time.
///
/// `GET /health` on `HEALTH_PORT` is served for the entire process lifetime and
/// reflects Postgres reachability and whether the persisted refresh token has
/// actually expired (a token never rotated yet — first boot, or waiting for
/// re-registration after a rejected one, NEX-107 — reads as healthy).
/// `POST /tokens` (used by `nexus saxo auth`) is a route on the same server.
///
/// The live WebSocket price stream was removed (NEX-193); nothing consumed it.
mod db;
mod kafka;

use alpha::saxo::{
    ChartClient, ChartError, InstrumentQuery, RotatedToken, SaxoAuth, SaxoToken, SharedToken,
    TokenStore, UicResolver,
};
use anyhow::Context;
use chronicle_lib::daily_bars::{self, BarSink, Instrument};
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
#[command(about = "Saxo token keeper and daily-bar sync — Deployment, not CronJob")]
struct Args {
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    #[arg(long, env = "KAFKA_BROKERS")]
    kafka_brokers: String,

    #[arg(
        long,
        env = "SAXO_API_BASE",
        default_value = "https://gateway.saxobank.com/openapi"
    )]
    saxo_api_base: String,

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

    #[arg(long, env = "HEALTH_PORT", default_value = "8080")]
    health_port: u16,

    /// Topic for official daily bars (stored by signal in `daily_bars`).
    #[arg(long, env = "DAILY_BARS_TOPIC", default_value = "market.daily_bars")]
    daily_bars_topic: String,

    /// MICs of the registered companies to sync daily bars for (comma separated).
    #[arg(
        long,
        env = "SAXO_MICS",
        value_delimiter = ',',
        default_value = "FNSE,XSTO,XCSE,FNDK,XHEL,FNFI"
    )]
    saxo_mics: Vec<String>,

    /// Overrides of the built-in Saxo ExchangeIds, `MIC=ExchangeId` comma
    /// separated (e.g. `XCSE=CSE,FNDK=CSE_FN-DK`), for when the built-in table
    /// (`alpha::saxo::exchange_ids`) does not match `GET /ref/v1/exchanges`.
    #[arg(long, env = "SAXO_EXCHANGE_IDS", value_delimiter = ',')]
    saxo_exchange_ids: Vec<String>,

    /// Saxo chart API version used for daily bars.
    #[arg(long, env = "SAXO_CHART_VERSION", default_value = "v1")]
    saxo_chart_version: String,
}

const SAXO_TOKEN_SOURCE: &str = "saxo";

/// Refresh the access token once it expires within this many seconds.
const TOKEN_REFRESH_THRESHOLD_SECS: i64 = 120;

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

/// Publishes daily bars to Kafka; `publish` returns once the broker acked.
struct KafkaBarSink {
    producer: Arc<ChronicleProducer>,
    topic: String,
}

#[async_trait::async_trait]
impl BarSink for KafkaBarSink {
    async fn publish(&self, bar: &model::generated::DailyBar) -> Result<(), String> {
        self.producer
            .publish_daily_bar(&self.topic, bar)
            .await
            .map_err(|e| e.to_string())
    }
}

/// Local time (Europe/Stockholm) of the daily refresh, after the 17:30 close.
const DAILY_BARS_HOUR: u32 = 18;
const DAILY_BARS_MINUTE: u32 = 30;
/// Minimum gap between chart requests (they count against Saxo's per-session
/// request limits).
const CHART_MIN_INTERVAL: Duration = Duration::from_secs(1);

/// First and longest wait before re-running a daily-bar sweep that failed,
/// instead of waiting for the next day's 18:30 run.
const SWEEP_RETRY_START: Duration = Duration::from_secs(15 * 60);
const SWEEP_RETRY_MAX: Duration = Duration::from_secs(2 * 60 * 60);

/// After a failure: how long to wait now (`current`, or Saxo's hint when that
/// is longer), and the backoff to use after the next failure (doubled, capped
/// at `max`).
fn backoff_step(current: Duration, max: Duration, hint: Option<Duration>) -> (Duration, Duration) {
    let wait = hint.map_or(current, |h| h.max(current));
    (wait, (current * 2).min(max))
}

/// Failed tickers of one sweep, and how many of those failures can go away on
/// a later try.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct SweepResult {
    failures: usize,
    transient: usize,
}

/// Whether a daily-bar sync failure can go away on its own. A chart error
/// says so itself; anything else (database, Kafka) is treated as transient.
fn is_transient_failure(e: &anyhow::Error) -> bool {
    e.chain()
        .find_map(|c| c.downcast_ref::<ChartError>())
        .is_none_or(ChartError::is_transient)
}

/// One pass over every registered Nordic instrument. Failures are logged and
/// counted and never propagate.
async fn daily_bars_sweep(
    pool: &sqlx::PgPool,
    resolver: &UicResolver,
    client: &mut ChartClient,
    sink: &dyn BarSink,
    shared_token: &SharedToken,
    mics: &[String],
) -> SweepResult {
    let tickers = match load_saxo_instruments(pool, mics).await {
        Ok(t) => t,
        Err(e) => {
            error!(error = %e, "daily bars: failed to load Nordic instruments");
            counter!("saxo_daily_bars_failures_total").increment(1);
            return SweepResult {
                failures: 1,
                transient: 1,
            };
        }
    };
    let mut out = SweepResult::default();
    for inst in &tickers {
        let ticker = &inst.ticker;
        let access_token = shared_token.lock().unwrap().access_token.clone();
        let resolved = match resolver
            .resolve_with_cfd_check(&inst.query(), &access_token)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                out.failures += 1;
                out.transient += usize::from(e.is_transient());
                counter!("saxo_daily_bars_failures_total").increment(1);
                error!(ticker, exchange_mic = %inst.exchange_mic, error = alpha::saxo::error_chain(&e),
                       transient = e.is_transient(), "daily bars: Uic resolution failed");
                continue;
            }
        };
        let instrument = Instrument {
            ticker: ticker.clone(),
            exchange_mic: inst.exchange_mic.clone(),
            uic: resolved.uic,
            // Saxo's currency, which is what the prices are quoted in.
            currency: resolved.currency,
        };
        if let Err(e) =
            daily_bars::sync_instrument(pool, client, sink, &instrument, Utc::now()).await
        {
            let transient = is_transient_failure(&e);
            out.failures += 1;
            out.transient += usize::from(transient);
            counter!("saxo_daily_bars_failures_total").increment(1);
            // `{:#}`: the whole anyhow chain, not only the outermost context.
            error!(
                ticker,
                exchange_mic = %inst.exchange_mic,
                error = format!("{e:#}"),
                transient,
                "daily bars: sync failed"
            );
        }
    }
    if out.failures == 0 && !tickers.is_empty() {
        gauge!("saxo_daily_bars_last_success_timestamp").set(Utc::now().timestamp() as f64);
    }
    out
}

/// Runs the daily-bar sync on startup and then every day at 18:30 Stockholm
/// time. Independent of the WebSocket loop: each sweep is its own task, so even
/// a panic can only end that sweep.
#[allow(clippy::too_many_arguments)]
async fn run_daily_bars(
    pool: sqlx::PgPool,
    http: reqwest::Client,
    resolver: Arc<UicResolver>,
    api_base: String,
    chart_version: String,
    shared_token: SharedToken,
    sink: Arc<KafkaBarSink>,
    mics: Vec<String>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut retry = SWEEP_RETRY_START;
    loop {
        let sweep = {
            let (pool, http, api_base, chart_version, shared_token, sink, resolver, mics) = (
                pool.clone(),
                http.clone(),
                api_base.clone(),
                chart_version.clone(),
                shared_token.clone(),
                sink.clone(),
                resolver.clone(),
                mics.clone(),
            );
            tokio::spawn(async move {
                let mut client =
                    ChartClient::new(http, &api_base, shared_token.clone(), CHART_MIN_INTERVAL)
                        .with_version(chart_version);
                daily_bars_sweep(
                    &pool,
                    &resolver,
                    &mut client,
                    sink.as_ref(),
                    &shared_token,
                    &mics,
                )
                .await
            })
        };
        // Retry early only if a failure may go away; permanent failures (a 4xx,
        // an unknown instrument) wait for the next scheduled run.
        let retry_early = match sweep.await {
            Ok(r) if r.failures == 0 => {
                info!("daily bars sweep finished");
                false
            }
            Ok(r) if r.transient > 0 => {
                warn!(
                    failed = r.failures,
                    transient = r.transient,
                    "daily bars sweep finished with failures"
                );
                true
            }
            Ok(r) => {
                error!(
                    failed = r.failures,
                    "daily bars sweep failed permanently, next try at the scheduled run"
                );
                false
            }
            Err(e) => {
                counter!("saxo_daily_bars_failures_total").increment(1);
                error!(error = %e, "daily bars sweep task died");
                true
            }
        };

        let wait = if !retry_early {
            retry = SWEEP_RETRY_START;
            let next = model::stockholm::next_stockholm_time(
                Utc::now(),
                DAILY_BARS_HOUR,
                DAILY_BARS_MINUTE,
            );
            info!(next = %next, "next daily bars sweep scheduled");
            (next - Utc::now())
                .to_std()
                .unwrap_or(Duration::from_secs(60))
        } else {
            let (wait, next_retry) = backoff_step(retry, SWEEP_RETRY_MAX, None);
            retry = next_retry;
            info!(retry_in = ?wait, "daily bars sweep had failures, retrying");
            wait
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = shutdown.changed() => return,
        }
    }
}

/// A registered company to stream and sync.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SaxoInstrument {
    ticker: String,
    exchange_mic: String,
    isin: Option<String>,
    currency: String,
}

impl SaxoInstrument {
    /// Key of the blocked-instrument map and of logs: `ticker` alone is not
    /// unique across exchanges.
    fn key(&self) -> String {
        format!("{}:{}", self.ticker, self.exchange_mic)
    }

    fn query(&self) -> InstrumentQuery<'_> {
        InstrumentQuery {
            ticker: &self.ticker,
            exchange_mic: &self.exchange_mic,
            isin: self.isin.as_deref(),
            registry_currency: Some(&self.currency),
        }
    }
}

/// Every registered company on one of `mics`, ordered by MIC then ticker.
async fn load_saxo_instruments(
    pool: &sqlx::PgPool,
    mics: &[String],
) -> anyhow::Result<Vec<SaxoInstrument>> {
    let rows = sqlx::query(
        "SELECT ticker, exchange_mic, isin, currency FROM companies \
         WHERE exchange_mic = ANY($1) ORDER BY exchange_mic, ticker",
    )
    .bind(mics)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| SaxoInstrument {
            ticker: r.get("ticker"),
            exchange_mic: r.get("exchange_mic"),
            isin: r.get("isin"),
            currency: r.get("currency"),
        })
        .collect())
}

/// Instrument count per MIC, in `mics` order, for the start-up log.
fn count_per_mic<'a>(mics: &'a [String], list: &[SaxoInstrument]) -> Vec<(&'a str, usize)> {
    mics.iter()
        .map(|m| {
            (
                m.as_str(),
                list.iter().filter(|i| &i.exchange_mic == m).count(),
            )
        })
        .collect()
}

/// Parses `MIC=ExchangeId` pairs.
fn parse_exchange_overrides(
    pairs: &[String],
) -> anyhow::Result<std::collections::HashMap<String, String>> {
    pairs
        .iter()
        .filter(|p| !p.trim().is_empty())
        .map(|p| {
            let (mic, id) = p
                .split_once('=')
                .with_context(|| format!("SAXO_EXCHANGE_IDS entry '{p}' is not MIC=ExchangeId"))?;
            Ok((mic.trim().to_string(), id.trim().to_string()))
        })
        .collect()
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

    let producer = Arc::new(
        ChronicleProducer::new(&args.kafka_brokers).context("failed to create Kafka producer")?,
    );

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
        "saxo_daily_bars_last_success_timestamp",
        "Unix time of the last daily-bar sweep in which every Nordic instrument \
         was fetched and published. Alert on staleness."
    );
    describe_counter!(
        "saxo_daily_bars_failures_total",
        "Daily-bar sync failures (Uic resolution, chart fetch, publish) since startup."
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

    let saxo_mics: Vec<String> = args
        .saxo_mics
        .iter()
        .map(|m| m.trim().to_uppercase())
        .filter(|m| !m.is_empty())
        .collect();
    let tickers = load_saxo_instruments(&pool, &saxo_mics)
        .await
        .unwrap_or_default();
    if tickers.is_empty() {
        warn!(
            mics = ?saxo_mics,
            "no Nordic instruments registered yet — register via: nexus register GOMX.ST; \
             each daily-bar sweep reloads the list"
        );
    } else {
        info!(count = tickers.len(), per_mic = ?count_per_mic(&saxo_mics, &tickers),
              tickers = ?tickers.iter().map(SaxoInstrument::key).collect::<Vec<_>>(),
              "loaded Nordic instruments");
    }

    let http = reqwest::Client::builder()
        .user_agent("nexus lasse.alm@gsfleet.io")
        .build()
        .context("failed to build HTTP client")?;

    // The daily-bar sweep's resolver; it caches Uics for the process lifetime.
    let uic_resolver = Arc::new(
        UicResolver::new(http.clone(), &args.saxo_api_base)
            .with_exchange_ids(parse_exchange_overrides(&args.saxo_exchange_ids)?),
    );
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
        let token_expires_at_unix = token_expires_at_unix.clone();
        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(30));
            // After a permanent refresh failure, try again only this often.
            let mut retry_at: Option<std::time::Instant> = None;
            loop {
                ticker.tick().await;
                if retry_at.is_some_and(|t| std::time::Instant::now() < t) {
                    continue;
                }

                let needs_refresh = shared_token
                    .lock()
                    .unwrap()
                    .expires_within_secs(TOKEN_REFRESH_THRESHOLD_SECS);
                if !needs_refresh {
                    continue;
                }

                match saxo_auth.refresh().await {
                    Ok(rotated) => {
                        retry_at = None;
                        // Drives both /health and the seconds-remaining metric.
                        token_expires_at_unix.store(
                            rotated.refresh_token_expires_at.timestamp(),
                            Ordering::Relaxed,
                        );
                        *shared_token.lock().unwrap() = rotated.access_token;
                        info!("Saxo access token rotated");
                    }
                    Err(e) if e.is_transient() => {
                        counter!("saxo_refresh_token_failures_total").increment(1);
                        error!(error = %e, "Saxo token refresh failed — will retry next tick");
                    }
                    Err(e) => {
                        counter!("saxo_refresh_token_failures_total").increment(1);
                        retry_at = Some(std::time::Instant::now() + Duration::from_secs(10 * 60));
                        error!(error = %e,
                               "Saxo rejected the token refresh — register a new token (nexus saxo auth); retrying every 10 min");
                    }
                }
            }
        });
    }

    tokio::spawn(run_daily_bars(
        pool.clone(),
        http.clone(),
        uic_resolver.clone(),
        args.saxo_api_base.clone(),
        args.saxo_chart_version.clone(),
        shared_token.clone(),
        Arc::new(KafkaBarSink {
            producer: producer.clone(),
            topic: args.daily_bars_topic.clone(),
        }),
        saxo_mics.clone(),
        shutdown_rx.clone(),
    ));

    info!("token refresh and daily-bar sync running");
    let mut shutdown = shutdown_rx.clone();
    while !*shutdown.borrow() {
        if shutdown.changed().await.is_err() {
            break;
        }
    }

    info!("saxo_stream shut down cleanly");
    Ok(())
}

#[cfg(test)]
mod backoff_tests {
    use super::*;

    #[test]
    fn instruments_are_keyed_by_ticker_and_mic_and_counted_per_mic() {
        let inst = |t: &str, m: &str| SaxoInstrument {
            ticker: t.into(),
            exchange_mic: m.into(),
            isin: None,
            currency: "SEK".into(),
        };
        let list = vec![
            inst("ABC", "XSTO"),
            inst("ABC", "XCSE"),
            inst("NOVO-B", "XCSE"),
        ];
        assert_ne!(list[0].key(), list[1].key());
        let mics: Vec<String> = ["FNSE", "XSTO", "XCSE", "FNDK"].map(String::from).to_vec();
        assert_eq!(
            count_per_mic(&mics, &list),
            vec![("FNSE", 0), ("XSTO", 1), ("XCSE", 2), ("FNDK", 0)]
        );
        assert_eq!(list[2].query().isin, None);
    }

    /// Every MIC the stream covers by default must have a built-in ExchangeId,
    /// or its companies could never be resolved.
    #[test]
    fn every_default_mic_has_a_saxo_exchange_id_and_a_time_zone() {
        use clap::Parser;
        let args = Args::parse_from([
            "saxo_stream",
            "--database-url=x",
            "--kafka-brokers=x",
            "--saxo-client-id=x",
            "--saxo-client-secret=x",
        ]);
        let mics: Vec<&str> = args.saxo_mics.iter().map(String::as_str).collect();
        assert_eq!(mics, ["FNSE", "XSTO", "XCSE", "FNDK", "XHEL", "FNFI"]);
        assert_eq!(mics, model::asset::mic::NORDIC);
        for m in mics {
            assert!(alpha::saxo::exchange_ids::default_for(m).is_some(), "{m}");
            assert!(model::exchange::timezone(m).is_some(), "{m}");
            assert!(model::asset::mic::currency(m).is_some(), "{m}");
        }
    }

    #[test]
    fn exchange_id_overrides_are_parsed() {
        let got =
            parse_exchange_overrides(&["XCSE=CSE".into(), " FNDK = CSE_FN-DK ".into()]).unwrap();
        assert_eq!(got["XCSE"], "CSE");
        assert_eq!(got["FNDK"], "CSE_FN-DK");
        assert!(parse_exchange_overrides(&["XCSE".into()]).is_err());
        assert!(parse_exchange_overrides(&[]).unwrap().is_empty());
    }

    #[test]
    fn chart_failures_are_classified_through_the_anyhow_chain() {
        let bad_request = anyhow::Error::new(ChartError::Status(reqwest::StatusCode::BAD_REQUEST))
            .context("chart request for GOMX");
        assert!(!is_transient_failure(&bad_request));
        let unavailable =
            anyhow::Error::new(ChartError::Status(reqwest::StatusCode::SERVICE_UNAVAILABLE))
                .context("chart request for GOMX");
        assert!(is_transient_failure(&unavailable));
        assert!(is_transient_failure(&anyhow::anyhow!("kafka down")));
    }

    #[test]
    fn backoff_doubles_to_the_cap() {
        let mut current = SWEEP_RETRY_START;
        let mut waits = Vec::new();
        for _ in 0..6 {
            let (wait, next) = backoff_step(current, SWEEP_RETRY_MAX, None);
            waits.push(wait.as_secs() / 60);
            current = next;
        }
        assert_eq!(waits, [15, 30, 60, 120, 120, 120]);
    }

    #[test]
    fn a_longer_saxo_hint_wins_but_never_shortens_the_wait() {
        let (wait, _) = backoff_step(
            Duration::from_secs(30),
            SWEEP_RETRY_MAX,
            Some(Duration::from_secs(3600)),
        );
        assert_eq!(wait.as_secs(), 3600);
        let (wait, _) = backoff_step(
            Duration::from_secs(120),
            SWEEP_RETRY_MAX,
            Some(Duration::from_secs(5)),
        );
        assert_eq!(wait.as_secs(), 120);
    }

    #[test]
    fn a_failed_sweep_is_retried_the_same_day() {
        let (wait, next) = backoff_step(SWEEP_RETRY_START, SWEEP_RETRY_MAX, None);
        assert_eq!((wait.as_secs(), next.as_secs()), (15 * 60, 30 * 60));
        let (wait, _) = backoff_step(SWEEP_RETRY_MAX, SWEEP_RETRY_MAX, None);
        assert_eq!(wait, SWEEP_RETRY_MAX);
    }
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
    fn health_is_ok_with_valid_unexpired_token() {
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
