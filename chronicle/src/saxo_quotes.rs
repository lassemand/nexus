//! saxo-stream's internal quote endpoint (NEX-249): `GET /quote?ticker=&mic=`.
//!
//! The one way the system reads live prices. It runs in saxo-stream because
//! that process alone holds the rotating Saxo token; the handler only reads the
//! current access token and never refreshes it.
//!
//! A quote never waits for anything but Saxo: Uics come from an in-memory map
//! that saxo-stream fills before the first request and on every company
//! reload. A pair that is not in the map answers 404 at once and is handed to
//! a background resolver through a bounded queue. A rate limit or a missing
//! token answers 503 with `retry_after_ms`; nothing is retried here.

use alpha::saxo::{QuoteClient, QuoteError, SaxoQuote, SharedToken};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{SecondsFormat, Utc};
use metrics::{counter, describe_counter, describe_histogram, histogram};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// `retry_after_ms` answered while saxo-stream has no valid access token.
pub const NO_TOKEN_RETRY_AFTER: Duration = Duration::from_millis(1000);

/// Buckets (seconds) of `saxo_quote_seconds` and `saxo_quote_upstream_seconds`.
pub const QUOTE_BUCKETS: &[f64] = &[
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.2, 0.3, 0.5, 1.0, 2.5,
];

/// Every `outcome` label value of `saxo_quote_requests_total`.
pub const OUTCOMES: [&str; 6] = [
    "ok",
    "not_found",
    "no_token",
    "rate_limited",
    "saxo_error",
    "bad_request",
];

/// A listing's Saxo identity, as the quote path needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuoteInstrument {
    /// Saxo's instrument id.
    pub uic: u64,
}

/// Key of the Uic map: upper-cased ticker and MIC.
pub fn key(ticker: &str, mic: &str) -> (String, String) {
    (ticker.trim().to_uppercase(), mic.trim().to_uppercase())
}

/// Uics of the registered companies, keyed by [`key`]. Written by the
/// resolver task, read by every quote.
pub type UicMap = Arc<RwLock<HashMap<(String, String), QuoteInstrument>>>;

/// The access token once saxo-stream has one. Empty until then; the quote
/// server is up before the token so callers get a 503, not a refused
/// connection.
pub type TokenSlot = Arc<OnceLock<SharedToken>>;

/// Everything a quote request needs.
pub struct QuoteState {
    /// The info-price client, with its own connection pool.
    pub client: QuoteClient,
    /// saxo-stream's token.
    pub token: TokenSlot,
    /// Resolved Uics.
    pub uics: UicMap,
    /// Pairs that were asked for but are not in [`Self::uics`], for the
    /// background resolver. Full or closed: the pair is dropped.
    pub unknown: mpsc::Sender<(String, String)>,
}

/// The query string of `GET /quote`.
#[derive(Debug, Deserialize)]
pub struct QuoteParams {
    ticker: Option<String>,
    mic: Option<String>,
}

/// The 200 body. Prices and sizes are decimal strings; anything Saxo omitted
/// is `null`.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct QuoteBody {
    /// Requested ticker, upper-cased.
    pub ticker: String,
    /// Requested MIC, upper-cased.
    pub mic: String,
    /// Saxo's instrument id the quote was read for.
    pub uic: u64,
    /// Best bid, the price a sale receives.
    pub bid: Option<String>,
    /// Best ask, the price a purchase pays.
    pub ask: Option<String>,
    /// Size at the best bid.
    pub bid_size: Option<String>,
    /// Size at the best ask.
    pub ask_size: Option<String>,
    /// Saxo's `Quote.PriceTypeBid`, unchanged.
    pub bid_type: Option<String>,
    /// Saxo's `Quote.PriceTypeAsk`, unchanged.
    pub ask_type: Option<String>,
    /// Saxo's `Quote.MarketState`, unchanged.
    pub market_state: Option<String>,
    /// Saxo's `Quote.DelayedByMinutes`.
    pub delayed_minutes: Option<i64>,
    /// Saxo's `DisplayAndFormat.Currency`.
    pub currency: Option<String>,
    /// Saxo's `LastUpdated`, as Saxo formatted it.
    pub saxo_last_updated: Option<String>,
    /// When saxo-stream had Saxo's answer (UTC, milliseconds).
    pub fetched_at: String,
    /// Saxo's round trip in milliseconds.
    pub saxo_ms: u64,
}

impl QuoteBody {
    fn new(ticker: String, mic: String, uic: u64, q: SaxoQuote, saxo_ms: u64) -> Self {
        let s = |d: Option<rust_decimal::Decimal>| d.map(|d| d.to_string());
        QuoteBody {
            ticker,
            mic,
            uic,
            bid: s(q.bid),
            ask: s(q.ask),
            bid_size: s(q.bid_size),
            ask_size: s(q.ask_size),
            bid_type: q.bid_type,
            ask_type: q.ask_type,
            market_state: q.market_state,
            delayed_minutes: q.delayed_minutes,
            currency: q.currency,
            saxo_last_updated: q.last_updated,
            fetched_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            saxo_ms,
        }
    }
}

/// The router serving `GET /quote`.
pub fn router(state: Arc<QuoteState>) -> Router {
    Router::new().route("/quote", get(quote)).with_state(state)
}

/// Registers the quote metrics at zero, with their descriptions.
pub fn describe() {
    for outcome in OUTCOMES {
        counter!("saxo_quote_requests_total", "outcome" => outcome).absolute(0);
    }
    describe_counter!(
        "saxo_quote_requests_total",
        "Requests to the internal quote endpoint, by outcome."
    );
    describe_histogram!(
        "saxo_quote_seconds",
        "Total time of a quote request inside saxo-stream, Saxo included."
    );
    describe_histogram!(
        "saxo_quote_upstream_seconds",
        "Time Saxo's info-price API took to answer a quote request."
    );
}

fn error(status: StatusCode, body: serde_json::Value) -> Response {
    (status, Json(body)).into_response()
}

fn retry(error_kind: &str, wait: Duration) -> Response {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        serde_json::json!({
            "error": error_kind,
            "retry_after_ms": u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
        }),
    )
}

async fn quote(
    State(state): State<Arc<QuoteState>>,
    Query(params): Query<QuoteParams>,
) -> Response {
    let started = Instant::now();
    let (outcome, response) = answer(&state, params).await;
    counter!("saxo_quote_requests_total", "outcome" => outcome).increment(1);
    histogram!("saxo_quote_seconds").record(started.elapsed().as_secs_f64());
    response
}

/// The current access token, if a valid one exists. The lock is held only to
/// clone the string.
fn access_token(slot: &TokenSlot) -> Option<String> {
    let token = slot.get()?.lock().unwrap_or_else(|e| e.into_inner());
    (token.expires_at > Utc::now() && !token.access_token.is_empty())
        .then(|| token.access_token.clone())
}

async fn answer(state: &QuoteState, params: QuoteParams) -> (&'static str, Response) {
    let (ticker, mic) = match (params.ticker, params.mic) {
        (Some(t), Some(m)) if !t.trim().is_empty() && !m.trim().is_empty() => key(&t, &m),
        _ => {
            return (
                "bad_request",
                error(
                    StatusCode::BAD_REQUEST,
                    serde_json::json!({"error": "ticker and mic are required"}),
                ),
            )
        }
    };

    let instrument = state
        .uics
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(ticker.clone(), mic.clone()))
        .cloned();
    let Some(instrument) = instrument else {
        if state
            .unknown
            .try_send((ticker.clone(), mic.clone()))
            .is_err()
        {
            debug!(ticker, mic, "background resolve queue full, pair dropped");
        }
        return (
            "not_found",
            error(
                StatusCode::NOT_FOUND,
                serde_json::json!({
                    "error": "not a registered company with a resolved Saxo Uic",
                    "ticker": ticker,
                    "mic": mic,
                }),
            ),
        );
    };

    let Some(token) = access_token(&state.token) else {
        return ("no_token", retry("no_token", NO_TOKEN_RETRY_AFTER));
    };

    let upstream = Instant::now();
    let fetched = state.client.fetch(instrument.uic, &token).await;
    histogram!("saxo_quote_upstream_seconds").record(upstream.elapsed().as_secs_f64());
    match fetched {
        Ok(f) => {
            let saxo_ms = u64::try_from(f.upstream.as_millis()).unwrap_or(u64::MAX);
            (
                "ok",
                Json(QuoteBody::new(
                    ticker,
                    mic,
                    instrument.uic,
                    f.quote,
                    saxo_ms,
                ))
                .into_response(),
            )
        }
        Err(QuoteError::RateLimited { retry_after }) => {
            warn!(ticker, mic, ?retry_after, "Saxo rate limited a quote");
            ("rate_limited", retry("rate_limited", retry_after))
        }
        Err(e) => {
            let transient = e.is_transient();
            let (saxo_status, body) = match &e {
                QuoteError::Status { status, body } => (Some(status.as_u16()), body.clone()),
                other => (None, alpha::saxo::error_chain(other)),
            };
            warn!(ticker, mic, saxo_status, transient, error = %body, "Saxo quote failed");
            (
                "saxo_error",
                error(
                    StatusCode::BAD_GATEWAY,
                    serde_json::json!({
                        "error": "saxo_error",
                        "saxo_status": saxo_status,
                        "body": body,
                        "transient": transient,
                    }),
                ),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alpha::saxo::SaxoToken;
    use std::sync::Mutex;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TRADABLE: &str = r#"{"AssetType":"Stock","Uic":4711,
        "LastUpdated":"2026-11-10T09:00:01.123Z",
        "DisplayAndFormat":{"Currency":"SEK","Decimals":2},
        "InstrumentPriceDetails":{"IsMarketOpen":true},
        "Quote":{"Bid":67.3,"Ask":67.4,"BidSize":1200,"AskSize":800,
                 "PriceTypeBid":"Tradable","PriceTypeAsk":"Tradable",
                 "MarketState":"Open","DelayedByMinutes":0}}"#;

    const NO_ACCESS: &str = r#"{"AssetType":"Stock","Uic":4711,
        "LastUpdated":"0001-01-01T00:00:00Z",
        "DisplayAndFormat":{"Currency":"SEK","Decimals":2},
        "Quote":{"PriceTypeBid":"NoAccess","PriceTypeAsk":"NoAccess",
                 "MarketState":"Open","DelayedByMinutes":15}}"#;

    struct Harness {
        base: String,
        http: reqwest::Client,
        unknown: mpsc::Receiver<(String, String)>,
        token: TokenSlot,
    }

    fn valid_token() -> SharedToken {
        Arc::new(Mutex::new(SaxoToken {
            access_token: "stream-token".into(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
        }))
    }

    async fn start(saxo: &MockServer, token: Option<SharedToken>) -> Harness {
        let slot: TokenSlot = Arc::new(OnceLock::new());
        if let Some(t) = token {
            let _ = slot.set(t);
        }
        let uics: UicMap = Arc::default();
        uics.write()
            .unwrap()
            .insert(key("ABC", "XSTO"), QuoteInstrument { uic: 4711 });
        let (tx, rx) = mpsc::channel(16);
        let state = Arc::new(QuoteState {
            client: QuoteClient::new(reqwest::Client::new(), saxo.uri()),
            token: slot.clone(),
            uics,
            unknown: tx,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router(state)).await });
        Harness {
            base: format!("http://{addr}"),
            http: reqwest::Client::new(),
            unknown: rx,
            token: slot,
        }
    }

    impl Harness {
        async fn get(&self, query: &str) -> (u16, serde_json::Value) {
            let resp = self
                .http
                .get(format!("{}/quote{query}", self.base))
                .send()
                .await
                .unwrap();
            let status = resp.status().as_u16();
            (status, resp.json().await.unwrap_or(serde_json::Value::Null))
        }
    }

    async fn saxo_answering(status: u16, body: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/trade/v1/infoprices"))
            .and(query_param("Uic", "4711"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn a_tradable_quote_returns_exact_decimal_strings() {
        let saxo = saxo_answering(200, TRADABLE).await;
        let h = start(&saxo, Some(valid_token())).await;
        let (status, body) = h.get("?ticker=abc&mic=XSTO").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["ticker"], "ABC");
        assert_eq!(body["mic"], "XSTO");
        assert_eq!(body["uic"], 4711);
        assert_eq!(body["bid"], "67.30");
        assert_eq!(body["ask"], "67.40");
        assert_eq!(body["bid_size"], "1200");
        assert_eq!(body["ask_size"], "800");
        assert_eq!(body["bid_type"], "Tradable");
        assert_eq!(body["ask_type"], "Tradable");
        assert_eq!(body["market_state"], "Open");
        assert_eq!(body["delayed_minutes"], 0);
        assert_eq!(body["currency"], "SEK");
        assert_eq!(body["saxo_last_updated"], "2026-11-10T09:00:01.123Z");
        assert!(body["fetched_at"].as_str().unwrap().ends_with('Z'));
        assert!(body["saxo_ms"].is_u64());
    }

    #[tokio::test]
    async fn no_access_is_passed_through_with_200_and_null_prices() {
        let saxo = saxo_answering(200, NO_ACCESS).await;
        let h = start(&saxo, Some(valid_token())).await;
        let (status, body) = h.get("?ticker=ABC&mic=XSTO").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["bid_type"], "NoAccess");
        assert_eq!(body["ask_type"], "NoAccess");
        assert_eq!(body["delayed_minutes"], 15);
        assert!(body["bid"].is_null());
        assert!(body["ask_size"].is_null());
    }

    #[tokio::test]
    async fn a_rate_limit_answers_503_with_the_wait_and_does_not_wait() {
        let saxo = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
            .expect(1)
            .mount(&saxo)
            .await;
        let h = start(&saxo, Some(valid_token())).await;
        let started = Instant::now();
        let (status, body) = h.get("?ticker=ABC&mic=XSTO").await;
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(status, 503);
        assert_eq!(body["retry_after_ms"], 1000);
    }

    #[tokio::test]
    async fn an_unauthorized_answer_is_a_bad_gateway_with_status_and_body() {
        let saxo = saxo_answering(401, r#"{"ErrorCode":"Unauthorized"}"#).await;
        let h = start(&saxo, Some(valid_token())).await;
        let (status, body) = h.get("?ticker=ABC&mic=XSTO").await;
        assert_eq!(status, 502);
        assert_eq!(body["saxo_status"], 401);
        assert_eq!(body["body"], r#"{"ErrorCode":"Unauthorized"}"#);
        assert_eq!(body["transient"], false);
    }

    #[tokio::test]
    async fn an_unknown_pair_is_404_and_queued_for_a_background_resolve() {
        let saxo = MockServer::start().await;
        let mut h = start(&saxo, Some(valid_token())).await;
        let (status, _) = h.get("?ticker=nope&mic=xcse").await;
        assert_eq!(status, 404);
        assert_eq!(
            h.unknown.try_recv().unwrap(),
            ("NOPE".to_string(), "XCSE".to_string())
        );
        assert!(saxo.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn missing_parameters_are_400() {
        let saxo = MockServer::start().await;
        let h = start(&saxo, Some(valid_token())).await;
        for q in ["", "?ticker=ABC", "?mic=XSTO", "?ticker=&mic=XSTO"] {
            assert_eq!(h.get(q).await.0, 400, "{q}");
        }
    }

    #[tokio::test]
    async fn no_token_yet_or_an_expired_one_is_503() {
        let saxo = saxo_answering(200, TRADABLE).await;
        let h = start(&saxo, None).await;
        let (status, body) = h.get("?ticker=ABC&mic=XSTO").await;
        assert_eq!(status, 503);
        assert_eq!(body["retry_after_ms"], 1000);
        let _ = h.token.set(Arc::new(Mutex::new(SaxoToken {
            access_token: "old".into(),
            expires_at: Utc::now() - chrono::Duration::seconds(1),
        })));
        assert_eq!(h.get("?ticker=ABC&mic=XSTO").await.0, 503);
        assert!(saxo.received_requests().await.unwrap().is_empty());
    }

    /// Our own overhead: end-to-end time through the real HTTP server against
    /// a Saxo mock that answers at once, p99 over 1000 sequential requests.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn overhead_without_saxo_is_p99_under_5_ms() {
        let saxo = saxo_answering(200, TRADABLE).await;
        let h = start(&saxo, Some(valid_token())).await;
        for _ in 0..20 {
            assert_eq!(h.get("?ticker=ABC&mic=XSTO").await.0, 200);
        }
        let mut times: Vec<Duration> = Vec::with_capacity(1000);
        for _ in 0..1000 {
            let started = Instant::now();
            let resp = h
                .http
                .get(format!("{}/quote?ticker=ABC&mic=XSTO", h.base))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            let _ = resp.bytes().await.unwrap();
            times.push(started.elapsed());
        }
        times.sort();
        let p99 = times[989];
        assert!(p99 < Duration::from_millis(5), "p99 {p99:?}");
    }
}
