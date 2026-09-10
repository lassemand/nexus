/// Persistent Saxo Bank WebSocket streaming ingestion service.
///
/// This is the only non-CronJob binary in `chronicle` — deployed as a
/// Kubernetes Deployment (single replica) because a WebSocket stream requires
/// a long-running process rather than a one-shot job.
///
/// # Health endpoint
///
/// `GET /health` on `HEALTH_PORT` (default 8080) is served for the entire
/// lifetime of the process — bound once at startup, before any Saxo token
/// exists, and never rebound. Its result is computed live from current
/// criteria each time it's hit, rather than depending on which startup
/// phase currently "owns" the port:
///
/// - The Postgres connection is reachable (a cheap bounded probe).
/// - The persisted Saxo refresh token has not actually expired. A pod that
///   has never rotated a token yet (fresh bootstrap, or recovering from a
///   stored token Saxo rejected — NEX-107) is treated as healthy, not
///   failing: it's waiting on the same fix (a human running `nexus saxo
///   auth`) in both cases, which is not a distinct failure mode for
///   readiness/liveness to gate on.
///
/// This is deliberately independent of whether any FNSE tickers are
/// registered or a WebSocket is currently connected — zero tickers is a
/// valid idle state (nothing to stream), not a failure, and must not
/// crashloop the pod or starve `/metrics` of Endpoints. WebSocket
/// connectivity is exposed separately as the `saxo_ws_connected` gauge on
/// `/metrics` instead of gating liveness/readiness (NEX-106).
///
/// `POST /tokens` (used by `nexus saxo auth` to deliver a freshly obtained
/// token) is a route on this same always-on server rather than a separate
/// temporary listener that swaps in and out depending on lifecycle phase.
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
///
/// If the stored refresh token is rejected by Saxo on startup (expired,
/// revoked, etc.), the process does NOT exit — restarting can never fix an
/// invalid external credential, so exiting would only crash-loop forever.
/// Instead it waits for a fresh token via the same `POST /tokens` route the
/// first-time bootstrap flow uses (NEX-107).
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

    /// OAuth2 client ID (from developer.saxo app registration).
    #[arg(long, env = "SAXO_CLIENT_ID")]
    saxo_client_id: String,

    /// OAuth2 client secret.
    #[arg(long, env = "SAXO_CLIENT_SECRET")]
    saxo_client_secret: String,

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

/// Whether the refresh-token cycle is healthy right now — one of the two
/// criteria `/health` combines (see `combine_health`). Deliberately does
/// NOT consider WebSocket/ticker state: a pod with zero FNSE tickers
/// registered has nothing to stream and is a valid idle state, not a
/// failure (NEX-106).
///
/// `expires_at_unix == 0` means no confirmed-valid token exists right now —
/// either a brief instant at first-ever startup, or waiting for
/// re-registration after a stored token was rejected (NEX-107) — treated as
/// healthy rather than failing the probe outright, since restarting the pod
/// cannot fix either case and the fix (a human running `nexus saxo auth`)
/// is already in progress.
fn refresh_token_healthy(token_expires_at_unix: &AtomicI64, now_unix: i64) -> bool {
    let expires_at = token_expires_at_unix.load(Ordering::Relaxed);
    expires_at == 0 || expires_at > now_unix
}

/// Cheap Postgres reachability probe for `/health`. Bounded by a short
/// timeout so a stalled connection can't hang the health endpoint itself —
/// a slow/unreachable database should read as unhealthy quickly, not make
/// liveness/readiness probes themselves time out.
async fn db_reachable(pool: &sqlx::PgPool) -> bool {
    tokio::time::timeout(
        Duration::from_secs(2),
        sqlx::query("SELECT 1").execute(pool),
    )
    .await
    .map(|result| result.is_ok())
    .unwrap_or(false)
}

/// Combines live health criteria into an HTTP status line + body. Kept as a
/// pure function separate from the criteria checks themselves (which are
/// async and I/O-bound) so "what counts as healthy, and why" is trivially
/// unit-testable without a real Postgres connection.
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

/// Flat JSON body accepted by `POST /tokens`.
///
/// `nexus saxo auth` sends this after completing the OAuth2 authorization-code
/// flow.  The struct mirrors what [`alpha::saxo::auth::SaxoAuth::exchange_code`]
/// returns so the CLI can forward the result without extra transformation.
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

/// Outcome of reading and validating a `POST /tokens` request body.
enum RegistrationOutcome {
    /// A valid, non-empty token pair was received.
    Registered(RotatedToken),
    /// The body was malformed, missing required fields, or unreadable — an
    /// appropriate 4xx has already been written to the client.
    Rejected,
}

/// Reads headers (for `Content-Length`) and body from `reader` — the request
/// line itself has already been consumed by the caller (`handle_connection`)
/// to route here — then parses and validates it as a [`TokenRegistrationBody`].
/// Writes the `200`/`400` response to `write_half` itself.
async fn read_registration_body(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    write_half: &mut OwnedWriteHalf,
) -> RegistrationOutcome {
    // Read headers; extract Content-Length.
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

    // Read exactly Content-Length bytes as the request body.
    let mut body = vec![0u8; content_length];
    if reader.read_exact(&mut body).await.is_err() {
        let _ = write_half
            .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
            .await;
        return RegistrationOutcome::Rejected;
    }

    // Deserialize and validate.
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

/// Handles one accepted connection: routes `GET /health`, `GET /metrics`,
/// and `POST /tokens` to their respective logic. This router runs for the
/// entire lifetime of the process — `/health` is always being served, its
/// result computed live from current criteria rather than which startup
/// phase happens to own the port (NEX-107 follow-up).
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
        // Recompute the countdown at scrape time so it is always fresh;
        // ws_connected is updated directly at state-change sites, so no
        // read-back is needed here.
        let now_unix = Utc::now().timestamp();
        let expires_at = token_expires_at_unix.load(Ordering::Relaxed);
        let seconds_remaining: f64 = if expires_at > 0 {
            (expires_at - now_unix).max(0) as f64
        } else {
            -1.0 // bootstrap sentinel: no rotation yet
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
            // If nobody is currently waiting for a registration (the stream
            // is already running fine), this is buffered (capacity 1)
            // rather than blocking this connection handler indefinitely.
            let _ = registration_tx.try_send(rotated);
        }
        return;
    }

    // Anything else targeting /tokens with the wrong method.
    if request_line.contains(" /tokens ") {
        let _ = write_half
            .write_all(
                b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nAllow: POST\r\n\r\n",
            )
            .await;
        return;
    }

    // Default: /health (and anything else) — mirrors the previous behavior
    // where any request that wasn't /metrics or /tokens-related was treated
    // as a liveness/readiness check, computed live from current criteria.
    let db_ok = db_reachable(&pool).await;
    let token_ok = refresh_token_healthy(&token_expires_at_unix, Utc::now().timestamp());
    let (status, body) = combine_health(db_ok, token_ok);
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n{body}",
        body.len()
    );
    let _ = write_half.write_all(response.as_bytes()).await;
}

/// Accept loop for the single long-lived HTTP endpoint. Spawns each
/// connection onto its own task so a slow client (or a `POST /tokens` sent
/// when nobody is currently waiting for one) can't stall `/health`/`/metrics`
/// probes from being served promptly.
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

/// Blocks until a token arrives via `POST /tokens` (handled by the
/// already-running `run_http_server`), logging a heartbeat every 60s so the
/// pod doesn't look hung in `kubectl logs` while waiting.
async fn wait_for_registration(
    registration_rx: &mut mpsc::Receiver<RotatedToken>,
) -> anyhow::Result<RotatedToken> {
    let mut heartbeat = interval(Duration::from_secs(60));
    heartbeat.tick().await; // consume the immediate first tick

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

/// Waits for a fresh token via `POST /tokens`, persists it, and builds the
/// `SaxoAuth` + initial access token the same way regardless of *why* we're
/// waiting (genuine first bootstrap, or a stored token that was rejected on
/// restart — NEX-107). The refresh token supplied here is already valid, so
/// — unlike the periodic rotation task — the caller must NOT call
/// `SaxoAuth::refresh()` on it immediately afterward (Saxo invalidates the
/// old one the instant it's used).
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

    // Persist the new refresh token before touching anything else.
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

    // Unix timestamp when the current refresh token expires.
    // 0 = no confirmed-valid token right now (startup, or waiting for
    // re-registration after a rejected one — see `refresh_token_healthy`).
    // Shared between the token-refresh task (writer) and the health
    // endpoint (reader).
    let token_expires_at_unix = Arc::new(AtomicI64::new(0));

    // Bind the health/metrics/tokens endpoint ONCE, before anything else
    // that could fail or block (Kafka, the Saxo token bootstrap, etc.) —
    // `/health` is served for the entire process lifetime, computed live
    // from current criteria rather than depending on which startup phase
    // owns the port.
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
        // ── Restart path ────────────────────────────────────────────────────
        // A refresh token exists from a previous run.  Call refresh() once to
        // derive a valid access token (Saxo never stores the access token —
        // only the refresh token is persisted).
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
                // The stored refresh token is dead (expired, revoked, or
                // otherwise rejected by Saxo). Restarting the process cannot
                // fix an invalid external credential, so propagating this
                // error out of `main` would only crash-loop forever against
                // the exact same failure. The health endpoint is already up
                // (bound above, before this ever ran) — it just needs to
                // know there's no confirmed-valid token right now, same as
                // the sentinel used during a genuine first-time bootstrap.
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
        // ── Fresh bootstrap path ─────────────────────────────────────────────
        // No token in DB.  Block until `nexus saxo auth` POSTs to /tokens via
        // the already-running health/metrics/tokens endpoint.
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

    // Not needed once a token is in hand: dropping it makes any further
    // `POST /tokens` (e.g. an operator retrying after the stream is already
    // running) fail fast on the sender side rather than sitting buffered or
    // blocking a connection-handler task forever.
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
                        // The refresh token itself was already rotated and
                        // persisted inside `refresh()` regardless of what
                        // happens next, so record its new expiry now — this is
                        // what drives both `/health` and the
                        // `saxo_refresh_token_seconds_remaining` metric, and
                        // must stay accurate even if WebSocket reauthorization
                        // below fails.
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

    // ── refresh_token_healthy: NEX-106 ────────────────────────────────────
    //
    // /health must reflect refresh-token state only, independent of
    // ws_healthy/ticker state — a pod with zero tickers registered must
    // stay healthy and must not crashloop.

    fn token_expiry(expires_at_unix: i64) -> AtomicI64 {
        AtomicI64::new(expires_at_unix)
    }

    #[test]
    fn health_is_ok_with_valid_unexpired_token_even_with_no_ws_connection() {
        let expiry = token_expiry(2_000_000_000); // far future
        assert!(
            refresh_token_healthy(&expiry, 1_000_000_000),
            "a valid, not-yet-expired refresh token must be healthy \
             regardless of WS/ticker state (no tickers registered case)"
        );
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

    // ── combine_health: pure criteria-combination logic ────────────────────

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
        assert!(body.contains("database unavailable"), "body: {body}");
        assert!(!body.contains("refresh token expired"), "body: {body}");
    }

    #[test]
    fn combine_health_is_503_when_token_expired() {
        let (status, body) = combine_health(true, false);
        assert_eq!(status, "503 Service Unavailable");
        assert!(body.contains("refresh token expired"), "body: {body}");
        assert!(!body.contains("database unavailable"), "body: {body}");
    }

    #[test]
    fn combine_health_reports_both_problems_when_both_fail() {
        let (status, body) = combine_health(false, false);
        assert_eq!(status, "503 Service Unavailable");
        assert!(body.contains("database unavailable"), "body: {body}");
        assert!(body.contains("refresh token expired"), "body: {body}");
    }

    // ── db_reachable ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn db_reachable_is_false_for_unreachable_database() {
        // Port 1 on loopback: nothing listens there, so the connection is
        // refused almost immediately — no real network/DNS dependency, so
        // this stays fast and deterministic in CI.
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
        // Never actually queried by tests that don't exercise the /health
        // route — connect_lazy avoids any real connection attempt at
        // construction time, so this is safe to use with no DB present.
        PgPoolOptions::new()
            .connect_lazy("postgres://user:pass@127.0.0.1:1/nonexistent")
            .expect("connect_lazy should not eagerly connect")
    }

    fn dummy_prometheus_handle() -> PrometheusHandle {
        PrometheusBuilder::new().build_recorder().handle()
    }

    // ── shared registration test helpers ────────────────────────────────────

    const VALID_ACCESS_TOKEN: &str = "acc_test_123";
    const VALID_REFRESH_TOKEN: &str = "ref_test_456";

    fn valid_body() -> String {
        format!(
            r#"{{"access_token":"{VALID_ACCESS_TOKEN}","refresh_token":"{VALID_REFRESH_TOKEN}","access_token_expires_at":"2099-01-01T00:00:00Z","refresh_token_expires_at":"2099-01-01T01:00:00Z"}}"#
        )
    }

    /// Full HTTP request bytes, request line included — for tests driving
    /// `handle_connection` (the full router).
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

    /// Headers + body only, no request line — for tests driving
    /// `read_registration_body` directly (the request line is consumed by
    /// `handle_connection` before that function is ever reached).
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

    /// Drive `read_registration_body` with the given raw "headers+body"
    /// bytes and return both the parsed outcome and the raw HTTP response.
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

    // ── read_registration_body: valid POST ─────────────────────────────────

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
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "expected 200, got: {response}"
        );
    }

    // ── read_registration_body: malformed / missing fields ─────────────────

    #[tokio::test]
    async fn read_registration_body_malformed_json_returns_rejected() {
        let (outcome, response) =
            registration_body_roundtrip(&headers_and_body("this is not json")).await;
        assert!(matches!(outcome, RegistrationOutcome::Rejected));
        assert!(response.contains("400"), "expected 400, got: {response}");
    }

    #[tokio::test]
    async fn read_registration_body_missing_access_token_returns_rejected() {
        let body = format!(
            r#"{{"refresh_token":"{VALID_REFRESH_TOKEN}","access_token_expires_at":"2099-01-01T00:00:00Z","refresh_token_expires_at":"2099-01-01T01:00:00Z"}}"#
        );
        let (outcome, response) = registration_body_roundtrip(&headers_and_body(&body)).await;
        assert!(matches!(outcome, RegistrationOutcome::Rejected));
        assert!(response.contains("400"), "expected 400, got: {response}");
    }

    #[tokio::test]
    async fn read_registration_body_empty_access_token_returns_rejected() {
        let body = format!(
            r#"{{"access_token":"","refresh_token":"{VALID_REFRESH_TOKEN}","access_token_expires_at":"2099-01-01T00:00:00Z","refresh_token_expires_at":"2099-01-01T01:00:00Z"}}"#
        );
        let (outcome, response) = registration_body_roundtrip(&headers_and_body(&body)).await;
        assert!(matches!(outcome, RegistrationOutcome::Rejected));
        assert!(response.contains("400"), "expected 400, got: {response}");
    }

    #[tokio::test]
    async fn read_registration_body_missing_refresh_token_returns_rejected() {
        let body = format!(
            r#"{{"access_token":"{VALID_ACCESS_TOKEN}","access_token_expires_at":"2099-01-01T00:00:00Z","refresh_token_expires_at":"2099-01-01T01:00:00Z"}}"#
        );
        let (outcome, response) = registration_body_roundtrip(&headers_and_body(&body)).await;
        assert!(matches!(outcome, RegistrationOutcome::Rejected));
        assert!(response.contains("400"), "expected 400, got: {response}");
    }

    // ── handle_connection: full router ──────────────────────────────────────

    /// Drive the full `handle_connection` router with raw request bytes
    /// (request line included) and a dummy pool/metrics/channel, returning
    /// the raw HTTP response. The dummy pool is only ever actually queried
    /// by the /health branch.
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

    /// GET /health with an unreachable dummy DB must report 503 mentioning
    /// the database problem — confirms the router actually wires
    /// `db_reachable` into `combine_health`, not just that the pure
    /// combination function works in isolation.
    #[tokio::test]
    async fn handle_connection_get_health_reports_db_unavailable() {
        let request = b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let (response, _rx) = connection_roundtrip(request).await;
        assert!(
            response.starts_with("HTTP/1.1 503"),
            "expected 503 with unreachable DB, got: {response}"
        );
        assert!(
            response.contains("database unavailable"),
            "expected DB problem in body, got: {response}"
        );
    }

    #[tokio::test]
    async fn handle_connection_get_metrics_returns_200() {
        let request = b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let (response, _rx) = connection_roundtrip(request).await;
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "expected 200, got: {response}"
        );
    }

    #[tokio::test]
    async fn handle_connection_get_tokens_returns_405() {
        let request = b"GET /tokens HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let (response, _rx) = connection_roundtrip(request).await;
        assert!(response.contains("405"), "expected 405, got: {response}");
    }

    /// A valid `POST /tokens` must both return 200 to the client and
    /// deliver the parsed token onto the registration channel for whoever
    /// (if anyone) is waiting — this is how `bootstrap_from_registration`
    /// receives it in `main`.
    #[tokio::test]
    async fn handle_connection_post_tokens_delivers_to_channel() {
        let body = valid_body();
        let (response, mut rx) = connection_roundtrip(&post_request(&body)).await;
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "expected 200, got: {response}"
        );
        let received = rx
            .try_recv()
            .expect("valid registration should be delivered to the channel");
        assert_eq!(received.access_token.access_token, VALID_ACCESS_TOKEN);
        assert_eq!(received.refresh_token, VALID_REFRESH_TOKEN);
    }

    // ── wait_for_registration ────────────────────────────────────────────────

    #[tokio::test]
    async fn wait_for_registration_blocks_then_returns_delivered_token() {
        let (tx, mut rx) = mpsc::channel::<RotatedToken>(1);
        let task = tokio::spawn(async move { wait_for_registration(&mut rx).await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !task.is_finished(),
            "wait_for_registration must block until a token is sent"
        );

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
        let result = wait_for_registration(&mut rx).await;
        assert!(result.is_err(), "closed channel should surface as an error");
    }

    // ── restart path: AC6 ─────────────────────────────────────────────────
    //
    // The restart path (stored refresh token → call refresh() once) requires a
    // real Postgres with a pre-seeded `oauth_tokens` row and is therefore
    // tagged #[ignore].  Run manually with:
    //   DATABASE_URL=postgres://... cargo test -p chronicle --bin saxo_stream \
    //     -- --ignored restart_path_calls_refresh_exactly_once
    //
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

        // Seed a refresh token row.
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

        // Wiremock for Saxo /token — should receive exactly one request.
        let saxo_mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"access_token":"new_acc","refresh_token":"new_ref","expires_in":1200,"refresh_token_expires_in":3600}"#,
            ))
            .expect(1)
            .mount(&saxo_mock)
            .await;

        // Simulate the restart path: load stored token → call refresh().
        let store = PgTokenStore { pool: pool.clone() };
        let stored = store
            .load_refresh_token()
            .await
            .expect("load_refresh_token failed");
        assert!(
            stored.is_some(),
            "test setup: expected a row in oauth_tokens"
        );

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

        // wiremock asserts `.expect(1)` on drop — verifies refresh() called once.
    }

    // ── NEX-107: expired-token recovery via bootstrap_from_registration ───
    //
    // Exercises the exact fallback `main()` now takes when the restart-path
    // `refresh()` call fails: instead of exiting, it calls
    // `bootstrap_from_registration`, which waits for a token on the
    // registration channel, persists the result, and returns a ready-to-use
    // `(SaxoToken, SaxoAuth)` pair — the same contract the fresh-bootstrap
    // path relies on. Requires a real Postgres for the persistence
    // assertion, so it's `#[ignore]`d like the sibling restart-path test
    // above. Run manually with:
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
        assert_ne!(
            token_expires_at_unix.load(Ordering::Relaxed),
            0,
            "refresh-token expiry must be recorded so /health reflects the new token"
        );

        // Persistence assertion: the new refresh token must be in oauth_tokens.
        let persisted = pg_store
            .load_refresh_token()
            .await
            .expect("load_refresh_token failed");
        assert_eq!(
            persisted.as_deref(),
            Some(VALID_REFRESH_TOKEN),
            "the recovered token must be persisted, same as the fresh-bootstrap path"
        );
    }
}
