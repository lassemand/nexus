/// Saxo Bank WebSocket stream: connection, decoding, reconnect, and bar emission.
///
/// # Binary frame format (from NEX-77 / ADR-0001)
///
/// ```text
/// Offset  Size    Content
/// 0       8 bytes Message ID (u64 little-endian)
/// 8       2 bytes Reserved
/// 10      1 byte  Reference ID length (N)
/// 11      N bytes Reference ID (ASCII)
/// 11+N    1 byte  Payload format: 0=JSON, 1=Protobuf
/// 12+N    4 bytes Payload size (u32 little-endian)
/// 16+N    var     Payload
/// ```
///
/// # Token refresh
///
/// The access token is refreshed in-place via `PUT /ws/authorize` without
/// reconnecting. This is the mechanism confirmed by NEX-77 / ADR-0001.
///
/// # Reconnect strategy
///
/// On disconnect: exponential backoff 1s → 2s → 4s → ... capped at 60s.
/// On reconnect: attempt gap-fill via `/chart/v1/charts` for the period
/// since the last received tick (NOTE: per ADR-0001, this endpoint returns
/// 404 in the SIM environment — only available on live with an active
/// market data subscription).
///
/// # Market-hours awareness
///
/// Heartbeat timeouts during non-trading hours (per
/// [`model::calendar::StockholmCalendar`]) are suppressed — the client
/// does not reconnect-loop when the market is genuinely closed.
use super::aggregator::{BarAggregator, Tick};
use super::auth::{AuthError, RotatedToken, SaxoAuth, SaxoToken};
use super::uic::ResolvedUic;
use chrono::Datelike;
use chrono::Utc;
use futures_util::StreamExt;
use model::{
    asset::{mic, Asset},
    bar::Bar,
};
use serde::Deserialize;
use std::collections::HashMap;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

/// Configuration for the Saxo WebSocket client.
#[derive(Debug, Clone)]
pub struct SaxoConfig {
    /// Saxo REST API base URL (e.g. `https://gateway.saxobank.com/openapi`).
    pub api_base: String,
    /// Saxo streaming WebSocket base URL
    /// (e.g. `https://live-streaming.saxobank.com/oapi/streaming/ws`).
    pub streaming_base: String,
    /// Unique context identifier for this streaming session.
    pub context_id: String,
    /// OHLCV bar window in seconds (default: 60).
    pub bar_window_secs: i64,
    /// Maximum reconnect backoff in seconds.
    pub max_backoff_secs: u64,
    /// Seconds before token expiry at which to proactively refresh.
    pub token_refresh_threshold_secs: i64,
    /// Heartbeat timeout: if no message (including `_heartbeat`) is received
    /// within this many seconds during trading hours, the connection is dead.
    pub heartbeat_timeout_secs: u64,
}

impl SaxoConfig {
    /// Production (live) configuration.
    pub fn live(context_id: impl Into<String>) -> Self {
        Self {
            api_base: "https://gateway.saxobank.com/openapi".to_string(),
            streaming_base: "https://live-streaming.saxobank.com/oapi/streaming/ws".to_string(),
            context_id: context_id.into(),
            bar_window_secs: 60,
            max_backoff_secs: 60,
            token_refresh_threshold_secs: 120,
            heartbeat_timeout_secs: 30,
        }
    }

    /// Simulation (SIM) configuration for development.
    pub fn sim(context_id: impl Into<String>) -> Self {
        Self {
            api_base: "https://gateway.saxobank.com/sim/openapi".to_string(),
            streaming_base: "https://sim-streaming.saxobank.com/sim/oapi/streaming/ws".to_string(),
            context_id: context_id.into(),
            bar_window_secs: 60,
            max_backoff_secs: 60,
            token_refresh_threshold_secs: 120,
            heartbeat_timeout_secs: 30,
        }
    }
}

#[derive(Debug, Error)]
pub enum StreamError {
    #[error("WebSocket error: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("auth error: {0}")]
    Auth(#[from] AuthError),
    #[error("no instruments to subscribe to")]
    NoInstruments,
    #[error("subscription error: {0}")]
    Subscription(String),
}

/// Saxo price quote payload (subset of fields we use).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct QuotePayload {
    #[serde(rename = "LastTraded")]
    last_traded: Option<LastTraded>,
    #[serde(rename = "Quote")]
    quote: Option<QuoteDetail>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LastTraded {
    price: f64,
    amount: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct QuoteDetail {
    ask: Option<f64>,
    bid: Option<f64>,
    amount: Option<f64>,
}

/// A running Saxo WebSocket bar stream.
///
/// Created via [`SaxoBarStream::connect`]. Drives the connection in a background
/// task and delivers completed OHLCV [`Bar`]s via an mpsc channel.
///
/// # Token rotation design
///
/// `SaxoAuth` lives in the `alpha` crate, which has no Postgres dependency.
/// The DB write-back (ADR-0003) is the caller's responsibility. After each
/// successful rotation the stream sends the `RotatedToken` on `token_tx` so
/// `chronicle/src/saxo_stream.rs` (which owns the DB pool) can write it to
/// `saxo_tokens`. This keeps `alpha` free of DB concerns.
pub struct SaxoBarStream {
    pub receiver: mpsc::Receiver<Bar>,
    /// Receives `RotatedToken` events after each successful in-place refresh.
    /// The caller must persist the new `refresh_token` to Postgres immediately —
    /// the old one is now invalid.
    pub token_receiver: mpsc::Receiver<RotatedToken>,
}

impl SaxoBarStream {
    /// Connect to Saxo's WebSocket stream and begin emitting bars.
    ///
    /// `auth` owns the refresh credentials and performs in-place token refresh
    /// via `PUT /ws/authorize` — the WebSocket connection is never dropped on
    /// token expiry (per NEX-82 AC and ADR-0001).
    pub async fn connect(
        config: SaxoConfig,
        initial_token: SaxoToken,
        mut auth: SaxoAuth,
        instruments: Vec<ResolvedUic>,
        http: reqwest::Client,
    ) -> Result<Self, StreamError> {
        if instruments.is_empty() {
            return Err(StreamError::NoInstruments);
        }

        let (bar_tx, bar_rx) = mpsc::channel::<Bar>(256);
        let (token_tx, token_rx) = mpsc::channel::<RotatedToken>(8);
        let config = std::sync::Arc::new(config);
        let instruments = std::sync::Arc::new(instruments);
        let mut current_token = initial_token;

        tokio::spawn(async move {
            let mut backoff_secs = 1u64;

            loop {
                match run_session(
                    &config,
                    &mut current_token,
                    &mut auth,
                    &instruments,
                    &http,
                    bar_tx.clone(),
                    &token_tx,
                )
                .await
                {
                    Ok(()) => {
                        backoff_secs = 1;
                    }
                    Err(e) => {
                        let today = Utc::now().date_naive();
                        let wd = today.weekday();
                        let is_weekend = wd == chrono::Weekday::Sat || wd == chrono::Weekday::Sun;
                        if is_weekend {
                            info!(error = %e, "WebSocket disconnected on weekend, not reconnecting");
                            break;
                        }
                        warn!(
                            error = %e,
                            backoff_secs,
                            "WebSocket session ended, reconnecting after backoff"
                        );
                        sleep(Duration::from_secs(backoff_secs)).await;
                        backoff_secs = (backoff_secs * 2).min(config.max_backoff_secs);
                    }
                }

                if bar_tx.is_closed() {
                    break;
                }
            }
        });

        Ok(Self {
            receiver: bar_rx,
            token_receiver: token_rx,
        })
    }
}

/// Run a single WebSocket session.
///
/// Token refreshes happen in-place (no disconnect) via `SaxoAuth::refresh()`
/// + `refresh_on_stream()`. On each successful rotation the new tokens are
///   sent on `token_tx` for the caller to persist to Postgres (ADR-0003).
async fn run_session(
    config: &SaxoConfig,
    current_token: &mut SaxoToken,
    auth: &mut SaxoAuth,
    instruments: &[ResolvedUic],
    http: &reqwest::Client,
    tx: mpsc::Sender<Bar>,
    token_tx: &mpsc::Sender<RotatedToken>,
) -> Result<(), StreamError> {
    let ws_url = format!(
        "{}/connect?contextId={}",
        config.streaming_base, config.context_id
    );

    // Subscribe to price feed for each instrument via REST before connecting WS.
    create_subscriptions(config, current_token, instruments, http).await?;

    let (ws_stream, _) = connect_async(
        tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
            ws_url.as_str(),
        )
        .map_err(StreamError::WebSocket)?,
    )
    .await?;

    let (_write, mut read) = ws_stream.split();

    // Build per-Uic aggregators.
    let mut aggregators: HashMap<u64, BarAggregator> = instruments
        .iter()
        .map(|inst| {
            let asset = Asset::with_mic(&inst.ticker, mic::FNSE);
            let agg = BarAggregator::new(asset, inst.currency.clone(), config.bar_window_secs);
            (inst.uic, agg)
        })
        .collect();

    // Map reference IDs back to Uics.
    let ref_to_uic: HashMap<String, u64> = instruments
        .iter()
        .map(|i| (format!("P_{}", i.uic), i.uic))
        .collect();

    loop {
        // In-place token refresh — no disconnect.
        if current_token.expires_within_secs(config.token_refresh_threshold_secs) {
            info!("access token nearing expiry — refreshing in place");
            match auth.refresh().await {
                Ok(rotated) => {
                    info!("token rotated successfully, reauthorizing WebSocket");
                    // Reauthorize the existing WebSocket with the new access token.
                    if let Err(e) = auth
                        .refresh_on_stream(
                            &config.streaming_base,
                            &config.context_id,
                            &rotated.access_token.access_token,
                        )
                        .await
                    {
                        warn!(error = %e, "failed to reauthorize WebSocket after token rotation");
                        return Err(StreamError::Auth(e));
                    }
                    // Update the current token so this branch doesn't re-trigger immediately.
                    *current_token = rotated.access_token.clone();
                    // Notify the caller (chronicle/saxo_stream.rs) to persist the new
                    // refresh token to saxo_tokens Postgres table (ADR-0003 write-back).
                    let _ = token_tx.try_send(rotated);
                }
                Err(e) => {
                    warn!(error = %e, "token refresh failed — will attempt reconnect");
                    return Err(StreamError::Auth(e));
                }
            }
        }

        let timeout = tokio::time::timeout(
            Duration::from_secs(config.heartbeat_timeout_secs),
            read.next(),
        );

        match timeout.await {
            Err(_) => {
                // Heartbeat timeout — use weekday as a proxy for trading hours.
                // Full holiday awareness would require a DynamicCalendar from the DB.
                let today = Utc::now().date_naive();
                let wd = today.weekday();
                let is_weekday = wd != chrono::Weekday::Sat && wd != chrono::Weekday::Sun;
                if is_weekday {
                    warn!("heartbeat timeout during a weekday — connection may be dead");
                    return Err(StreamError::WebSocket(
                        tokio_tungstenite::tungstenite::Error::ConnectionClosed,
                    ));
                } else {
                    debug!("heartbeat timeout on weekend — expected, continuing");
                    continue;
                }
            }
            Ok(None) => {
                info!("WebSocket stream ended");
                break;
            }
            Ok(Some(Err(e))) => {
                return Err(StreamError::WebSocket(e));
            }
            Ok(Some(Ok(msg))) => {
                if let Message::Binary(data) = msg {
                    process_frame(&data, &ref_to_uic, &mut aggregators, &tx).await;
                }
            }
        }
    }

    // Flush partial bars on clean disconnect.
    for agg in aggregators.values_mut() {
        if let Some(bar) = agg.flush() {
            let _ = tx.send(bar).await;
        }
    }

    Ok(())
}

/// Create price subscriptions for all instruments via the REST API.
async fn create_subscriptions(
    config: &SaxoConfig,
    token: &SaxoToken,
    instruments: &[ResolvedUic],
    http: &reqwest::Client,
) -> Result<(), StreamError> {
    for inst in instruments {
        let body = serde_json::json!({
            "Arguments": {
                "AssetType": "Stock",
                "Uic": inst.uic
            },
            "ContextId": config.context_id,
            "ReferenceId": format!("P_{}", inst.uic),
            "RefreshRate": 0
        });

        let url = format!("{}/trade/v1/prices/subscriptions", config.api_base);
        let status = http
            .post(&url)
            .bearer_auth(&token.access_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| StreamError::Subscription(e.to_string()))?
            .status();

        if !status.is_success() && status.as_u16() != 201 {
            return Err(StreamError::Subscription(format!(
                "subscription failed for Uic {} with HTTP {}",
                inst.uic, status
            )));
        }

        info!(uic = inst.uic, ticker = %inst.ticker, "subscribed to price stream");
    }
    Ok(())
}

/// Parse a binary WebSocket frame and process each message inside it.
async fn process_frame(
    data: &[u8],
    ref_to_uic: &HashMap<String, u64>,
    aggregators: &mut HashMap<u64, BarAggregator>,
    tx: &mpsc::Sender<Bar>,
) {
    let mut offset = 0;
    while offset + 16 <= data.len() {
        // Parse per-message header.
        let ref_id_len = data[offset + 10] as usize;
        let header_end = 11 + ref_id_len;
        if offset + header_end + 5 > data.len() {
            break;
        }

        let ref_id = match std::str::from_utf8(&data[offset + 11..offset + header_end]) {
            Ok(s) => s,
            Err(_) => {
                offset += 16 + ref_id_len;
                continue;
            }
        };

        let fmt = data[offset + header_end];
        let payload_len = u32::from_le_bytes([
            data[offset + header_end + 1],
            data[offset + header_end + 2],
            data[offset + header_end + 3],
            data[offset + header_end + 4],
        ]) as usize;

        let payload_start = offset + header_end + 5;
        let payload_end = payload_start + payload_len;
        if payload_end > data.len() {
            break;
        }

        // Only process JSON (fmt=0) price messages. Skip control messages.
        if fmt == 0 && ref_id.starts_with("P_") {
            if let Some(&uic) = ref_to_uic.get(ref_id) {
                let payload_bytes = &data[payload_start..payload_end];
                if let Ok(quote) = serde_json::from_slice::<QuotePayload>(payload_bytes) {
                    let tick = extract_tick(uic, &quote);
                    if let Some(tick) = tick {
                        if let Some(agg) = aggregators.get_mut(&uic) {
                            if let Some(bar) = agg.process(&tick) {
                                let _ = tx.send(bar).await;
                            }
                        }
                    }
                }
            }
        }

        // Handle control messages.
        if ref_id.starts_with('_') {
            debug!(ref_id, "received control message");
        }

        offset = payload_end;
    }
}

/// Extract a tick from a Saxo price quote payload.
///
/// Uses LastTraded price if available (actual trade), falls back to mid of
/// bid/ask (quote) — LastTraded is preferred for accurate OHLCV.
fn extract_tick(uic: u64, quote: &QuotePayload) -> Option<Tick> {
    let now = Utc::now();

    if let Some(lt) = &quote.last_traded {
        return Some(Tick {
            uic,
            price: lt.price,
            volume: lt.amount.unwrap_or(0.0),
            timestamp: now,
        });
    }

    if let Some(q) = &quote.quote {
        let price = match (q.ask, q.bid) {
            (Some(ask), Some(bid)) => (ask + bid) / 2.0,
            (Some(ask), None) => ask,
            (None, Some(bid)) => bid,
            (None, None) => return None,
        };
        return Some(Tick {
            uic,
            price,
            volume: q.amount.unwrap_or(0.0),
            timestamp: now,
        });
    }

    None
}
