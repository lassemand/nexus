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
/// Heartbeat timeouts on days when **none of the subscribed exchanges** is open
/// (per [`super::MarketCalendar`]) are suppressed — the client does not
/// reconnect-loop when the markets are genuinely closed. One exchange being
/// open (say Copenhagen on Swedish Midsummer Eve) keeps the check on.
///
/// # Market-data entitlement
///
/// Saxo marks each quote with a price quality (`Quote.PriceTypeBid/Ask`, where
/// `NoAccess` means the account has no feed for the instrument) and
/// `Quote.DelayedByMinutes`. Each instrument's state is reported to an
/// [`AccessObserver`]; a `NoAccess` instrument simply produces no ticks.
use super::aggregator::{BarAggregator, Tick};
use super::auth::{AuthError, SaxoToken, SharedToken};
use super::market_calendar::MarketCalendar;
use super::uic::ResolvedUic;
use chrono::Utc;
use futures_util::StreamExt;
use model::{asset::Asset, bar::Bar};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
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
    WebSocket(Box<tokio_tungstenite::tungstenite::Error>),
    #[error("auth error: {0}")]
    Auth(#[from] AuthError),
    #[error("no instruments to subscribe to")]
    NoInstruments,
    #[error("subscription error: {0}")]
    Subscription(String),
}

impl From<tokio_tungstenite::tungstenite::Error> for StreamError {
    fn from(e: tokio_tungstenite::tungstenite::Error) -> Self {
        StreamError::WebSocket(Box::new(e))
    }
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
    /// `Indicative`, `OldIndicative`, `NoAccess`, `NoMarket`, `Pending`, ...
    price_type_ask: Option<String>,
    price_type_bid: Option<String>,
    /// 0 = real time; more is a delayed feed.
    delayed_by_minutes: Option<u32>,
}

/// Whether the Saxo account gets prices for an instrument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataAccess {
    /// Real-time prices (also when the market is closed).
    Realtime,
    /// A delayed feed (`DelayedByMinutes > 0`).
    Delayed,
    /// No price feed for the instrument (`PriceType` = `NoAccess`).
    NoAccess,
}

impl DataAccess {
    /// Label value of `saxo_instrument_data_access`.
    pub fn as_str(self) -> &'static str {
        match self {
            DataAccess::Realtime => "realtime",
            DataAccess::Delayed => "delayed",
            DataAccess::NoAccess => "no_access",
        }
    }

    /// All states, for gauges that set the current one to 1 and the rest to 0.
    pub const ALL: [DataAccess; 3] = [
        DataAccess::Realtime,
        DataAccess::Delayed,
        DataAccess::NoAccess,
    ];
}

/// Entitlement of a quote, if it says anything. A quote without price-type or
/// delay fields (a bare last-trade update) says nothing.
fn classify_access(q: &QuoteDetail) -> Option<DataAccess> {
    let no_access = |t: &Option<String>| t.as_deref() == Some("NoAccess");
    if no_access(&q.price_type_ask) || no_access(&q.price_type_bid) {
        return Some(DataAccess::NoAccess);
    }
    match q.delayed_by_minutes {
        Some(m) if m > 0 => Some(DataAccess::Delayed),
        Some(_) => Some(DataAccess::Realtime),
        None if q.price_type_ask.is_some() || q.price_type_bid.is_some() => {
            Some(DataAccess::Realtime)
        }
        None => None,
    }
}

/// Told when an instrument's market-data entitlement is first seen or changes
/// within a stream session.
pub trait AccessObserver: Send + Sync {
    /// `ticker`/`exchange_mic` are the registry's.
    fn observe(&self, ticker: &str, exchange_mic: &str, state: DataAccess);
}

/// An observer that ignores everything.
pub struct NoAccessObserver;

impl AccessObserver for NoAccessObserver {
    fn observe(&self, _: &str, _: &str, _: DataAccess) {}
}

/// Per-session state shared by the frame decoder.
struct SessionState {
    aggregators: HashMap<u64, BarAggregator>,
    ref_to_uic: HashMap<String, u64>,
    listings: HashMap<u64, (String, String)>,
    access: HashMap<u64, DataAccess>,
    observer: Arc<dyn AccessObserver>,
}

/// A running Saxo WebSocket bar stream.
///
/// Created via [`SaxoBarStream::connect`]. Drives the connection in a background
/// task and delivers completed OHLCV [`Bar`]s via an mpsc channel.
///
/// # Token rotation design
///
/// Token refresh is *not* handled here. A separate periodic task (owned by
/// the caller — see `chronicle/src/saxo_stream.rs`) refreshes the access
/// token in-place via `SaxoAuth::refresh()` + `refresh_on_stream()` and
/// writes the result into the shared [`SharedToken`] passed to `connect`.
/// This stream only *reads* that shared state (once per connect/reconnect,
/// for REST subscription calls) — it never drives refresh itself, and the
/// read loop below never needs to break for token reasons.
pub struct SaxoBarStream {
    pub receiver: mpsc::Receiver<Bar>,
}

impl SaxoBarStream {
    /// Connect to Saxo's WebSocket stream and begin emitting bars.
    ///
    /// `shared_token` is read fresh on every connect/reconnect attempt, so
    /// this always uses whatever the caller's periodic refresh task most
    /// recently rotated to.
    pub async fn connect(
        config: SaxoConfig,
        shared_token: SharedToken,
        instruments: Vec<ResolvedUic>,
        http: reqwest::Client,
        calendar: Arc<MarketCalendar>,
        observer: Arc<dyn AccessObserver>,
    ) -> Result<Self, StreamError> {
        if instruments.is_empty() {
            return Err(StreamError::NoInstruments);
        }

        let (bar_tx, bar_rx) = mpsc::channel::<Bar>(256);
        let config = std::sync::Arc::new(config);
        let instruments = std::sync::Arc::new(instruments);

        tokio::spawn(async move {
            let mut backoff_secs = 1u64;

            loop {
                let token_snapshot = shared_token.lock().unwrap().clone();

                match run_session(
                    &config,
                    &token_snapshot,
                    &instruments,
                    &http,
                    bar_tx.clone(),
                    &calendar,
                    &observer,
                )
                .await
                {
                    Ok(()) => {
                        backoff_secs = 1;
                    }
                    Err(e) => {
                        if !calendar.any_open_at(
                            instruments.iter().map(|i| i.exchange_mic.as_str()),
                            Utc::now(),
                        ) {
                            info!(error = %e, "WebSocket disconnected while every exchange is closed, not reconnecting");
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

        Ok(Self { receiver: bar_rx })
    }
}

/// Run a single WebSocket session.
///
/// Does not refresh tokens itself — the access token is fixed for the
/// lifetime of one session (refresh happens out-of-band via a REST call
/// keyed by `context_id`, independent of any specific WebSocket object, so
/// there is nothing for this loop to react to).
async fn run_session(
    config: &SaxoConfig,
    token: &SaxoToken,
    instruments: &[ResolvedUic],
    http: &reqwest::Client,
    tx: mpsc::Sender<Bar>,
    calendar: &MarketCalendar,
    observer: &Arc<dyn AccessObserver>,
) -> Result<(), StreamError> {
    let ws_url = format!(
        "{}/connect?contextId={}",
        config.streaming_base, config.context_id
    );

    // Subscribe to price feed for each instrument via REST before connecting WS.
    let subscribed = create_subscriptions(config, token, instruments, http).await?;
    let instruments = &subscribed[..];

    let (ws_stream, _) = connect_async(
        tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
            ws_url.as_str(),
        )
        .map_err(|e| StreamError::WebSocket(Box::new(e)))?,
    )
    .await?;

    let (_write, mut read) = ws_stream.split();

    // Per-Uic aggregators; each bar carries the instrument's own MIC and
    // Saxo's currency.
    let mut state = SessionState {
        aggregators: instruments
            .iter()
            .map(|inst| {
                let asset = Asset::with_mic(&inst.ticker, &inst.exchange_mic);
                let agg = BarAggregator::new(asset, inst.currency.clone(), config.bar_window_secs);
                (inst.uic, agg)
            })
            .collect(),
        // Reference IDs back to Uics.
        ref_to_uic: instruments
            .iter()
            .map(|i| (format!("P_{}", i.uic), i.uic))
            .collect(),
        listings: instruments
            .iter()
            .map(|i| (i.uic, (i.ticker.clone(), i.exchange_mic.clone())))
            .collect(),
        access: HashMap::new(),
        observer: observer.clone(),
    };

    loop {
        let timeout = tokio::time::timeout(
            Duration::from_secs(config.heartbeat_timeout_secs),
            read.next(),
        );

        match timeout.await {
            Err(_) => {
                // Heartbeat timeout: only a problem if some subscribed exchange
                // is open today (weekends and every holiday in `calendar`).
                let any_open = calendar.any_open_at(
                    instruments.iter().map(|i| i.exchange_mic.as_str()),
                    Utc::now(),
                );
                if any_open {
                    warn!("heartbeat timeout on a trading day — connection may be dead");
                    return Err(StreamError::WebSocket(Box::new(
                        tokio_tungstenite::tungstenite::Error::ConnectionClosed,
                    )));
                } else {
                    debug!(
                        "heartbeat timeout while every exchange is closed — expected, continuing"
                    );
                    continue;
                }
            }
            Ok(None) => {
                info!("WebSocket stream ended");
                break;
            }
            Ok(Some(Err(e))) => {
                return Err(StreamError::WebSocket(Box::new(e)));
            }
            Ok(Some(Ok(msg))) => {
                if let Message::Binary(data) = msg {
                    process_frame(&data, &mut state, &tx).await;
                }
            }
        }
    }

    // Flush partial bars on clean disconnect.
    for agg in state.aggregators.values_mut() {
        if let Some(bar) = agg.flush() {
            let _ = tx.send(bar).await;
        }
    }

    Ok(())
}

/// Create price subscriptions for all instruments via the REST API and return
/// the ones that are subscribed.
///
/// A permanent rejection (a 4xx other than 408/429) skips that instrument with
/// an error log, so one instrument Saxo refuses (say, no entitlement) cannot
/// keep the others from streaming or cause a reconnect loop. A transient
/// failure fails the session as before, to be retried.
async fn create_subscriptions(
    config: &SaxoConfig,
    token: &SaxoToken,
    instruments: &[ResolvedUic],
    http: &reqwest::Client,
) -> Result<Vec<ResolvedUic>, StreamError> {
    let mut subscribed = Vec::with_capacity(instruments.len());
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
            if super::is_transient_status(status) {
                return Err(StreamError::Subscription(format!(
                    "subscription failed for Uic {} with HTTP {}",
                    inst.uic, status
                )));
            }
            tracing::error!(uic = inst.uic, ticker = %inst.ticker, exchange_mic = %inst.exchange_mic,
                %status, "Saxo refused the price subscription, instrument skipped");
            continue;
        }

        info!(uic = inst.uic, ticker = %inst.ticker, exchange_mic = %inst.exchange_mic,
              "subscribed to price stream");
        subscribed.push(inst.clone());
    }
    if subscribed.is_empty() {
        return Err(StreamError::Subscription(
            "Saxo refused every price subscription".to_string(),
        ));
    }
    Ok(subscribed)
}

/// Parse a binary WebSocket frame and process each message inside it.
async fn process_frame(data: &[u8], state: &mut SessionState, tx: &mpsc::Sender<Bar>) {
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
            if let Some(&uic) = state.ref_to_uic.get(ref_id) {
                let payload_bytes = &data[payload_start..payload_end];
                if let Ok(quote) = serde_json::from_slice::<QuotePayload>(payload_bytes) {
                    observe_access(state, uic, &quote);
                    let tick = extract_tick(uic, &quote);
                    if let Some(tick) = tick {
                        if let Some(agg) = state.aggregators.get_mut(&uic) {
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

/// Reports an instrument's entitlement when first seen or changed.
fn observe_access(state: &mut SessionState, uic: u64, quote: &QuotePayload) {
    let Some(access) = quote.quote.as_ref().and_then(classify_access) else {
        return;
    };
    if state.access.insert(uic, access) == Some(access) {
        return;
    }
    if let Some((ticker, mic)) = state.listings.get(&uic) {
        state.observer.observe(ticker, mic, access);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn quote(json: &str) -> QuotePayload {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn no_access_is_parsed_without_error_and_yields_no_tick() {
        let q = quote(
            r#"{"Quote":{"PriceTypeAsk":"NoAccess","PriceTypeBid":"NoAccess","DelayedByMinutes":0,
                "ErrorCode":"None","MarketState":"Open"}}"#,
        );
        assert_eq!(
            q.quote.as_ref().and_then(classify_access),
            Some(DataAccess::NoAccess)
        );
        assert!(extract_tick(1, &q).is_none());
    }

    #[test]
    fn realtime_and_delayed_are_told_apart() {
        let rt = quote(
            r#"{"Quote":{"Ask":10.5,"Bid":10.0,"PriceTypeAsk":"Indicative","PriceTypeBid":"OldIndicative","DelayedByMinutes":0}}"#,
        );
        let delayed = quote(
            r#"{"Quote":{"Ask":10.5,"Bid":10.0,"PriceTypeAsk":"Indicative","DelayedByMinutes":15}}"#,
        );
        let bare = quote(r#"{"LastTraded":{"Price":10.0}}"#);
        assert_eq!(
            rt.quote.as_ref().and_then(classify_access),
            Some(DataAccess::Realtime)
        );
        assert_eq!(
            delayed.quote.as_ref().and_then(classify_access),
            Some(DataAccess::Delayed)
        );
        assert_eq!(bare.quote.as_ref().and_then(classify_access), None);
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<(String, String, DataAccess)>>);
    impl AccessObserver for Recorder {
        fn observe(&self, t: &str, m: &str, s: DataAccess) {
            self.0.lock().unwrap().push((t.into(), m.into(), s));
        }
    }

    #[test]
    fn the_observer_hears_an_instrument_once_per_state() {
        let rec = Arc::new(Recorder::default());
        let mut state = SessionState {
            aggregators: HashMap::new(),
            ref_to_uic: HashMap::new(),
            listings: [(7, ("NOVO-B".to_string(), "XCSE".to_string()))].into(),
            access: HashMap::new(),
            observer: rec.clone(),
        };
        let na = quote(r#"{"Quote":{"PriceTypeBid":"NoAccess","DelayedByMinutes":0}}"#);
        observe_access(&mut state, 7, &na);
        observe_access(&mut state, 7, &na);
        let rt = quote(r#"{"Quote":{"PriceTypeBid":"Indicative","DelayedByMinutes":0}}"#);
        observe_access(&mut state, 7, &rt);
        let seen = rec.0.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![
                ("NOVO-B".into(), "XCSE".into(), DataAccess::NoAccess),
                ("NOVO-B".into(), "XCSE".into(), DataAccess::Realtime),
            ]
        );
    }
}
