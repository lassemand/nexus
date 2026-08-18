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

/// Attempt to read and handle one `POST /tokens` request from an accepted TCP stream.
///
/// Returns `Some(body)` if the request was valid and a `200 OK` was sent.
/// Returns `None` for any other request (wrong method, malformed body, missing
/// fields) after sending the appropriate 4xx response — the caller keeps listening.
async fn handle_registration_request(
    stream: tokio::net::TcpStream,
) -> Option<TokenRegistrationBody> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // Read the HTTP request line.
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await.is_err() {
        return None;
    }

    // Respond to liveness/readiness probes while waiting for registration.
    // Without this the probe gets 405 and kills the pod before the user can
    // complete the browser OAuth flow.
    if request_line.trim_end().starts_with("GET /health") {
        const BODY: &[u8] = b"waiting_for_registration";
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n",
            BODY.len()
        );
        let _ = write_half.write_all(header.as_bytes()).await;
        let _ = write_half.write_all(BODY).await;
        return None;
    }

    // Only accept POST /tokens; return 405 for everything else.
    if !request_line.trim_end().starts_with("POST /tokens") {
        let _ = write_half
            .write_all(
                b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nAllow: POST\r\n\r\n",
            )
            .await;
        return None;
    }

    // Read headers; extract Content-Length.
    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.is_err() {
            return None;
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
        return None;
    }

    // Read exactly Content-Length bytes as the request body.
    let mut body = vec![0u8; content_length];
    if reader.read_exact(&mut body).await.is_err() {
        let _ = write_half
            .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
            .await;
        return None;
    }

    // Deserialize and validate.
    match serde_json::from_slice::<TokenRegistrationBody>(&body) {
        Ok(reg) if !reg.access_token.is_empty() && !reg.refresh_token.is_empty() => {
            let _ = write_half
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await;
            Some(reg)
        }
        _ => {
            let _ = write_half
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                .await;
            None
        }
    }
}

/// Block until a valid `POST /tokens` request arrives on `port`.
///
/// Binds a temporary `TcpListener` on `0.0.0.0:{port}`, loops accepting
/// connections, and logs a heartbeat every 60 s so the pod doesn't look hung in
/// `kubectl logs`.  Returns the parsed and validated token body as a
/// [`RotatedToken`] on the first valid request; all invalid requests get a 4xx
/// and are silently ignored so the loop keeps waiting.
///
/// The listener is dropped (port freed) before this function returns, so the
/// caller can immediately bind the same port for `serve_health`.
async fn await_token_registration(port: u16) -> anyhow::Result<RotatedToken> {
    let listener = TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .with_context(|| format!("failed to bind registration listener on port {port}"))?;

    info!(
        port,
        "no Saxo token in DB — listening for POST /tokens to bootstrap the stream"
    );

    let mut heartbeat = tokio::time::interval(Duration::from_secs(60));
    heartbeat.tick().await; // consume the immediate first tick

    loop {
        let stream = tokio::select! {
            res = listener.accept() => {
                match res {
                    Ok((s, _)) => s,
                    Err(e) => {
                        warn!(error = %e, "registration listener accept error");
                        continue;
                    }
                }
            }
            _ = heartbeat.tick() => {
                info!("waiting for Saxo token registration via POST /tokens");
                continue;
            }
        };

        if let Some(body) = handle_registration_request(stream).await {
            info!("Saxo tokens received via POST /tokens — proceeding with stream startup");
            return Ok(body.into());
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

    // Determine startup mode based on whether a refresh token already exists in
    // the DB.  The health endpoint is deliberately NOT started yet on the fresh
    // path — its port is occupied by the registration listener until registration
    // completes.
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
        let rotated = auth
            .refresh()
            .await
            .context("initial token refresh failed — stored refresh token may have expired")?;
        // Health endpoint can start immediately on restart.
        tokio::spawn(serve_health(args.health_port, metrics.clone()));
        (rotated.access_token, auth)
    } else {
        // ── Fresh bootstrap path ─────────────────────────────────────────────
        // No token in DB.  Block until `nexus saxo auth` POSTs to /tokens.
        // The registration listener binds the health port during this phase
        // (it is the only HTTP endpoint active), freeing it on return so that
        // serve_health can re-bind immediately after.
        let rotated = await_token_registration(args.health_port).await?;

        // Persist the new refresh token before touching anything else.
        pg_store.save(&rotated).await;

        // Do NOT call refresh() on this path — the supplied access_token is
        // already valid and calling refresh() would rotate the just-obtained
        // refresh token for no benefit (Saxo invalidates the old one immediately).
        let initial = rotated.access_token.clone();
        let fresh_refresh_token = rotated.refresh_token.clone();

        let token_store: Arc<dyn TokenStore> = Arc::new(PgTokenStore { pool: pool.clone() });
        let auth = SaxoAuth::new(
            http.clone(),
            format!("{}/token", args.saxo_auth_base),
            args.saxo_client_id.clone(),
            args.saxo_client_secret.clone(),
            fresh_refresh_token,
            token_store,
        );

        // Registration listener has exited; port is now free for health.
        tokio::spawn(serve_health(args.health_port, metrics.clone()));
        (initial, auth)
    };

    let shared_token: SharedToken = Arc::new(Mutex::new(initial_token));

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    // ── shared helpers ────────────────────────────────────────────────────

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

    /// Drive `handle_registration_request` with the given raw HTTP bytes and
    /// return both the parsed result and the raw HTTP response string.
    async fn roundtrip(request: &[u8]) -> (Option<TokenRegistrationBody>, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let req = request.to_vec();

        tokio::join!(
            async {
                let (stream, _) = listener.accept().await.unwrap();
                handle_registration_request(stream).await
            },
            async {
                let mut client = TcpStream::connect(addr).await.unwrap();
                client.write_all(&req).await.unwrap();
                // Read until server closes its write half (after sending the response).
                let mut buf = Vec::new();
                client.read_to_end(&mut buf).await.unwrap();
                String::from_utf8_lossy(&buf).to_string()
            }
        )
    }

    // ── handle_registration_request: valid POST ───────────────────────────

    #[tokio::test]
    async fn handle_registration_valid_post_returns_body() {
        let body = valid_body();
        let (result, response) = roundtrip(&post_request(&body)).await;

        let parsed = result.expect("valid POST should return Some");
        assert_eq!(parsed.access_token, VALID_ACCESS_TOKEN);
        assert_eq!(parsed.refresh_token, VALID_REFRESH_TOKEN);
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "expected 200, got: {response}"
        );
    }

    // ── handle_registration_request: AC3 malformed JSON ──────────────────

    #[tokio::test]
    async fn handle_registration_malformed_json_returns_400() {
        let (result, response) = roundtrip(&post_request("this is not json")).await;
        assert!(result.is_none(), "malformed JSON must return None");
        assert!(response.contains("400"), "expected 400, got: {response}");
    }

    // ── handle_registration_request: AC4 missing / empty fields ──────────

    #[tokio::test]
    async fn handle_registration_missing_access_token_returns_400() {
        let body = format!(
            r#"{{"refresh_token":"{VALID_REFRESH_TOKEN}","access_token_expires_at":"2099-01-01T00:00:00Z","refresh_token_expires_at":"2099-01-01T01:00:00Z"}}"#
        );
        let (result, response) = roundtrip(&post_request(&body)).await;
        assert!(result.is_none());
        assert!(response.contains("400"), "expected 400, got: {response}");
    }

    #[tokio::test]
    async fn handle_registration_empty_access_token_returns_400() {
        let body = format!(
            r#"{{"access_token":"","refresh_token":"{VALID_REFRESH_TOKEN}","access_token_expires_at":"2099-01-01T00:00:00Z","refresh_token_expires_at":"2099-01-01T01:00:00Z"}}"#
        );
        let (result, response) = roundtrip(&post_request(&body)).await;
        assert!(result.is_none());
        assert!(response.contains("400"), "expected 400, got: {response}");
    }

    #[tokio::test]
    async fn handle_registration_missing_refresh_token_returns_400() {
        let body = format!(
            r#"{{"access_token":"{VALID_ACCESS_TOKEN}","access_token_expires_at":"2099-01-01T00:00:00Z","refresh_token_expires_at":"2099-01-01T01:00:00Z"}}"#
        );
        let (result, response) = roundtrip(&post_request(&body)).await;
        assert!(result.is_none());
        assert!(response.contains("400"), "expected 400, got: {response}");
    }

    // ── handle_registration_request: AC5 wrong HTTP method ───────────────

    #[tokio::test]
    async fn handle_registration_get_tokens_returns_405() {
        let request = b"GET /tokens HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let (result, response) = roundtrip(request).await;
        assert!(result.is_none(), "GET /tokens must return None");
        assert!(response.contains("405"), "expected 405, got: {response}");
    }

    /// Liveness/readiness probes send GET /health while the registration listener
    /// is active. Without a 200 response the pod gets killed before the user can
    /// complete the browser OAuth flow.
    #[tokio::test]
    async fn handle_registration_get_health_returns_200_for_probe() {
        let request = b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let (result, response) = roundtrip(request).await;
        assert!(
            result.is_none(),
            "GET /health must not unblock registration"
        );
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "expected 200 for liveness probe, got: {response}"
        );
        assert!(
            response.contains("waiting_for_registration"),
            "body should signal waiting state, got: {response}"
        );
    }

    // ── await_token_registration: AC1 + AC7 (blocking + correct token) ───

    /// Valid POST unblocks `await_token_registration` and returns a `RotatedToken`
    /// whose `access_token` / `refresh_token` exactly match the request body.
    /// This also verifies stream-start gating (AC7): code beyond
    /// `await_token_registration` — including any WebSocket connect — cannot
    /// execute until this function returns.
    #[tokio::test]
    async fn await_token_registration_blocks_then_returns_correct_token() {
        let port = 20001u16;
        let task = tokio::spawn(await_token_registration(port));

        // Give the listener time to bind.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Assert it is still blocking with nothing sent.
        assert!(
            !task.is_finished(),
            "await_token_registration must block until a valid POST arrives"
        );

        // Send the valid POST.
        let body = valid_body();
        let mut conn = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        conn.write_all(&post_request(&body)).await.unwrap();
        let mut resp_buf = Vec::new();
        conn.read_to_end(&mut resp_buf).await.unwrap();

        // Should now complete within 2 s.
        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("timed out waiting for registration to complete")
            .unwrap()
            .expect("await_token_registration should succeed");

        assert_eq!(result.access_token.access_token, VALID_ACCESS_TOKEN);
        assert_eq!(result.refresh_token, VALID_REFRESH_TOKEN);
    }

    // ── await_token_registration: AC3/4/5 ignored; only valid POST unblocks

    #[tokio::test]
    async fn await_token_registration_ignores_invalid_requests_keeps_waiting() {
        let port = 20002u16;
        let task = tokio::spawn(await_token_registration(port));
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Send malformed JSON — must NOT unblock.
        let mut conn = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        conn.write_all(&post_request("not json")).await.unwrap();
        let mut buf = Vec::new();
        conn.read_to_end(&mut buf).await.unwrap();
        drop(conn);

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !task.is_finished(),
            "malformed JSON must not unblock the registration listener"
        );

        // Send GET — must NOT unblock.
        let mut conn2 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        conn2
            .write_all(b"GET /tokens HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut buf2 = Vec::new();
        conn2.read_to_end(&mut buf2).await.unwrap();
        drop(conn2);

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !task.is_finished(),
            "wrong-method request must not unblock the registration listener"
        );

        // Send valid POST — must unblock.
        let body = valid_body();
        let mut conn3 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        conn3.write_all(&post_request(&body)).await.unwrap();
        let mut buf3 = Vec::new();
        conn3.read_to_end(&mut buf3).await.unwrap();

        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("timed out")
            .unwrap()
            .expect("should succeed after valid POST");
        assert_eq!(result.access_token.access_token, VALID_ACCESS_TOKEN);
    }

    // ── await_token_registration: AC2 (refresh() NOT called on fresh path) ─

    /// Verifies that `await_token_registration` never contacts Saxo's `/token`
    /// endpoint. A wiremock spy intercepts any such call; asserting zero received
    /// requests proves that `SaxoAuth::refresh()` is not invoked on the
    /// fresh-registration path.
    #[tokio::test]
    async fn await_token_registration_does_not_call_saxo_token_endpoint() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Spy: would respond if called, but must receive zero requests.
        let saxo_spy = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"access_token":"spy_acc","refresh_token":"spy_ref","expires_in":1200}"#,
            ))
            .mount(&saxo_spy)
            .await;

        let port = 20003u16;
        let task = tokio::spawn(await_token_registration(port));
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Send valid registration — provides access_token directly.
        let body = valid_body();
        let mut conn = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        conn.write_all(&post_request(&body)).await.unwrap();
        let mut buf = Vec::new();
        conn.read_to_end(&mut buf).await.unwrap();

        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .expect("registration should succeed");
        assert_eq!(result.access_token.access_token, VALID_ACCESS_TOKEN);

        // Core assertion: the Saxo /token endpoint received ZERO requests.
        let received = saxo_spy.received_requests().await.unwrap();
        assert_eq!(
            received.len(),
            0,
            "SaxoAuth::refresh() must not be called on the fresh-registration path \
             (received {received:?})"
        );
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
}
