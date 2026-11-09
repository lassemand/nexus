//! Live Level 1 quotes from Saxo's info-price API (`GET /trade/v1/infoprices`).
//!
//! # Read only
//!
//! [`QuoteClient`] can build exactly one kind of URL, an info-price lookup for
//! one Uic. It has no way to reach an order, position or account endpoint, and
//! none may be added here: the analyst's trades are simulated.
//!
//! # Token constraint
//!
//! Like [`super::chart::ChartClient`], the caller passes the access token it
//! read from saxo-stream's [`super::SharedToken`]; this client never
//! authenticates.
//!
//! # Speed
//!
//! One request, no throttle, no sleep and no retry: a rate limit comes back as
//! [`QuoteError::RateLimited`] with Saxo's `Retry-After`, and the caller
//! decides whether to try again.
//!
//! # Prices are decimals
//!
//! Prices and sizes are parsed from the raw JSON number text straight into
//! [`Decimal`]; they never pass through `f64`. A price with fewer digits than
//! `DisplayAndFormat.Decimals` is padded with zeros (`67.3` → `67.30`); digits
//! are never rounded away.
//!
//! # Field names
//!
//! `Quote.{Bid,Ask,BidSize,AskSize,PriceTypeBid,PriceTypeAsk,MarketState,DelayedByMinutes}`,
//! `LastUpdated` and `DisplayAndFormat.{Currency,Decimals}` follow Saxo's
//! reference documentation. The first raw response of the process is logged
//! at `debug` so they can be checked against the live gateway.

use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::value::RawValue;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use thiserror::Error;
use tracing::debug;

/// The only API path this client calls.
pub const INFOPRICES_PATH: &str = "/trade/v1/infoprices";
/// Field groups requested with every quote.
pub const FIELD_GROUPS: &str = "Quote,DisplayAndFormat,InstrumentPriceDetails";
/// Longest wait a rate limit is reported with.
pub const MAX_RETRY_AFTER: Duration = Duration::from_millis(2000);
/// Wait reported for a 429 that carries no usable `Retry-After`.
pub const DEFAULT_RETRY_AFTER: Duration = Duration::from_millis(1000);
/// How many bytes of an error body are kept.
pub const ERROR_BODY_BYTES: usize = 300;

static FIRST_RESPONSE_LOGGED: AtomicBool = AtomicBool::new(false);

/// Why a quote could not be read.
#[derive(Debug, Error)]
pub enum QuoteError {
    /// Saxo answered 429; try again after `retry_after` (capped at
    /// [`MAX_RETRY_AFTER`]).
    #[error("Saxo rate limited the quote request, retry after {retry_after:?}")]
    RateLimited { retry_after: Duration },
    /// Saxo answered another non-2xx status; `body` is the start of its body.
    #[error("Saxo returned {status} for the quote: {body}")]
    Status {
        status: reqwest::StatusCode,
        body: String,
    },
    /// No HTTP response (connect, TLS, timeout, body read).
    #[error("quote request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// A 200 whose body is not an info price.
    #[error("undecodable quote response: {0}")]
    Decode(String),
    /// The configured API base is not a URL.
    #[error("invalid quote URL: {0}")]
    Url(#[from] url::ParseError),
}

impl QuoteError {
    /// Whether the same request can succeed later (no response, 408, 429, 5xx).
    pub fn is_transient(&self) -> bool {
        match self {
            QuoteError::RateLimited { .. } => true,
            QuoteError::Status { status, .. } => super::is_transient_status(*status),
            QuoteError::Http(e) => super::is_transient_reqwest(e),
            QuoteError::Decode(_) | QuoteError::Url(_) => false,
        }
    }
}

/// One info price as Saxo sent it. Every field Saxo omits (or sends as
/// `null`) is `None`; the strings are Saxo's own, unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaxoQuote {
    /// Best bid.
    pub bid: Option<Decimal>,
    /// Best ask.
    pub ask: Option<Decimal>,
    /// Size at the best bid.
    pub bid_size: Option<Decimal>,
    /// Size at the best ask.
    pub ask_size: Option<Decimal>,
    /// `Quote.PriceTypeBid`, e.g. `Tradable`, `Indicative`, `NoAccess`.
    pub bid_type: Option<String>,
    /// `Quote.PriceTypeAsk`.
    pub ask_type: Option<String>,
    /// `Quote.MarketState`, e.g. `Open`, `Closed`.
    pub market_state: Option<String>,
    /// `Quote.DelayedByMinutes`.
    pub delayed_minutes: Option<i64>,
    /// `DisplayAndFormat.Currency`.
    pub currency: Option<String>,
    /// `DisplayAndFormat.Decimals`.
    pub decimals: Option<u32>,
    /// `LastUpdated`, as Saxo formatted it.
    pub last_updated: Option<String>,
}

/// A quote and how long Saxo took to answer it.
#[derive(Debug, Clone)]
pub struct FetchedQuote {
    /// The parsed quote.
    pub quote: SaxoQuote,
    /// Time from sending the request to having the full body.
    pub upstream: Duration,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawInfoPrice {
    #[serde(default)]
    quote: Option<RawQuote>,
    #[serde(default)]
    display_and_format: Option<RawDisplay>,
    #[serde(default)]
    last_updated: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawQuote {
    #[serde(default)]
    bid: Option<Box<RawValue>>,
    #[serde(default)]
    ask: Option<Box<RawValue>>,
    #[serde(default)]
    bid_size: Option<Box<RawValue>>,
    #[serde(default)]
    ask_size: Option<Box<RawValue>>,
    #[serde(default)]
    price_type_bid: Option<String>,
    #[serde(default)]
    price_type_ask: Option<String>,
    #[serde(default)]
    market_state: Option<String>,
    #[serde(default)]
    delayed_by_minutes: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawDisplay {
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    decimals: Option<u32>,
}

fn raw_decimal(raw: &Option<Box<RawValue>>) -> Option<Decimal> {
    let text = raw.as_ref()?.get().trim();
    Decimal::from_str(text)
        .or_else(|_| Decimal::from_scientific(text))
        .ok()
}

/// `value` with at least `decimals` digits after the point; never rounded.
fn pad_to(value: Decimal, decimals: Option<u32>) -> Decimal {
    match decimals {
        Some(d) if value.scale() < d && d <= 28 => {
            let mut v = value;
            v.rescale(d);
            v
        }
        _ => value,
    }
}

/// Parses an info-price body. Missing or non-numeric values become `None`.
pub fn parse_info_price(body: &str) -> Result<SaxoQuote, serde_json::Error> {
    let raw: RawInfoPrice = serde_json::from_str(body)?;
    let (currency, decimals) = raw
        .display_and_format
        .map(|d| (d.currency, d.decimals))
        .unwrap_or((None, None));
    let q = raw.quote;
    let price = |f: fn(&RawQuote) -> &Option<Box<RawValue>>| {
        q.as_ref()
            .and_then(|q| raw_decimal(f(q)))
            .map(|v| pad_to(v, decimals))
    };
    let size =
        |f: fn(&RawQuote) -> &Option<Box<RawValue>>| q.as_ref().and_then(|q| raw_decimal(f(q)));
    Ok(SaxoQuote {
        bid: price(|q| &q.bid),
        ask: price(|q| &q.ask),
        bid_size: size(|q| &q.bid_size),
        ask_size: size(|q| &q.ask_size),
        bid_type: q.as_ref().and_then(|q| q.price_type_bid.clone()),
        ask_type: q.as_ref().and_then(|q| q.price_type_ask.clone()),
        market_state: q.as_ref().and_then(|q| q.market_state.clone()),
        delayed_minutes: q.as_ref().and_then(|q| q.delayed_by_minutes),
        currency,
        decimals,
        last_updated: raw.last_updated,
    })
}

/// `Retry-After` in seconds (integer or fractional), capped at
/// [`MAX_RETRY_AFTER`]; [`DEFAULT_RETRY_AFTER`] when absent or unreadable.
pub fn retry_after(headers: &reqwest::header::HeaderMap) -> Duration {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|s| s.is_finite() && *s >= 0.0)
        .map(|s| Duration::from_secs_f64(s.min(MAX_RETRY_AFTER.as_secs_f64())))
        .unwrap_or(DEFAULT_RETRY_AFTER)
        .min(MAX_RETRY_AFTER)
}

fn truncate_utf8(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Reads one info price per call. Holds its own keep-alive HTTP client so
/// quotes never queue behind the chart client's throttle.
#[derive(Clone)]
pub struct QuoteClient {
    http: reqwest::Client,
    api_base: String,
}

impl QuoteClient {
    /// `api_base` is e.g. `https://gateway.saxobank.com/openapi`. `http` should
    /// be dedicated to quotes (its connection pool stays warm).
    pub fn new(http: reqwest::Client, api_base: impl Into<String>) -> Self {
        Self {
            http,
            api_base: api_base.into().trim_end_matches('/').to_string(),
        }
    }

    /// The info-price URL for `uic`, the only URL this client can build.
    pub fn infoprices_url(&self, uic: u64) -> Result<url::Url, QuoteError> {
        let mut url = url::Url::parse(&format!("{}{INFOPRICES_PATH}", self.api_base))?;
        url.query_pairs_mut()
            .append_pair("Uic", &uic.to_string())
            .append_pair("AssetType", "Stock")
            .append_pair("FieldGroups", FIELD_GROUPS);
        Ok(url)
    }

    /// Fetches the current quote of the stock `uic` with `access_token`. One
    /// request, never retried and never delayed.
    pub async fn fetch(&self, uic: u64, access_token: &str) -> Result<FetchedQuote, QuoteError> {
        let url = self.infoprices_url(uic)?;
        let started = Instant::now();
        let sent = self.http.get(url).bearer_auth(access_token).send().await;
        super::api_metrics::record(
            super::api_metrics::Endpoint::InfoPrices,
            started,
            sent.as_ref().ok().map(|r| r.status()),
        );
        let resp = sent?;
        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(QuoteError::RateLimited {
                retry_after: retry_after(resp.headers()),
            });
        }
        let body = resp.text().await?;
        let upstream = started.elapsed();
        if !status.is_success() {
            return Err(QuoteError::Status {
                status,
                body: truncate_utf8(&body, ERROR_BODY_BYTES).to_string(),
            });
        }
        if !FIRST_RESPONSE_LOGGED.swap(true, Ordering::Relaxed) {
            debug!(uic, body = %body, "first Saxo info-price response");
        }
        let quote = parse_info_price(&body).map_err(|e| {
            QuoteError::Decode(format!("{e}: {}", truncate_utf8(&body, ERROR_BODY_BYTES)))
        })?;
        Ok(FetchedQuote { quote, upstream })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TRADABLE: &str = r#"{"AssetType":"Stock","Uic":4711,
        "LastUpdated":"2026-11-10T09:00:01.123000Z",
        "DisplayAndFormat":{"Currency":"SEK","Decimals":2,"Symbol":"ABC:xsto"},
        "InstrumentPriceDetails":{"IsMarketOpen":true},
        "Quote":{"Bid":67.3,"Ask":67.4,"BidSize":1200.0,"AskSize":800,"Mid":67.35,
                 "PriceTypeBid":"Tradable","PriceTypeAsk":"Tradable",
                 "MarketState":"Open","DelayedByMinutes":0,"ErrorCode":"None"}}"#;

    #[test]
    fn prices_keep_their_digits_and_are_padded_to_display_decimals() {
        let q = parse_info_price(TRADABLE).unwrap();
        assert_eq!(q.bid.unwrap().to_string(), "67.30");
        assert_eq!(q.ask.unwrap().to_string(), "67.40");
        assert_eq!(q.bid_size.unwrap().to_string(), "1200.0");
        assert_eq!(q.ask_size.unwrap().to_string(), "800");
        assert_eq!(q.bid_type.as_deref(), Some("Tradable"));
        assert_eq!(q.market_state.as_deref(), Some("Open"));
        assert_eq!(q.delayed_minutes, Some(0));
        assert_eq!(q.currency.as_deref(), Some("SEK"));
        assert_eq!(q.decimals, Some(2));
        assert_eq!(
            q.last_updated.as_deref(),
            Some("2026-11-10T09:00:01.123000Z")
        );
    }

    #[test]
    fn values_an_f64_would_corrupt_come_back_exactly() {
        let body = r#"{"Quote":{"Bid":0.30000000000000004,"Ask":67.3,"BidSize":1e3}}"#;
        let q = parse_info_price(body).unwrap();
        assert_eq!(q.bid.unwrap().to_string(), "0.30000000000000004");
        assert_eq!(q.ask.unwrap().to_string(), "67.3");
        assert_eq!(q.bid_size.unwrap().to_string(), "1000");
        let body = r#"{"DisplayAndFormat":{"Decimals":2},"Quote":{"Bid":12.345}}"#;
        assert_eq!(
            parse_info_price(body).unwrap().bid.unwrap().to_string(),
            "12.345"
        );
    }

    #[test]
    fn omitted_and_null_values_are_none() {
        let q = parse_info_price(r#"{"Quote":{"Bid":null,"PriceTypeAsk":"NoAccess"}}"#).unwrap();
        assert_eq!(q.bid, None);
        assert_eq!(q.ask, None);
        assert_eq!(q.ask_type.as_deref(), Some("NoAccess"));
        assert_eq!(q.currency, None);
        let q = parse_info_price("{}").unwrap();
        assert_eq!(q.market_state, None);
        assert_eq!(q.last_updated, None);
    }

    #[test]
    fn retry_after_is_read_in_seconds_and_capped() {
        let h = |v: &str| {
            let mut m = reqwest::header::HeaderMap::new();
            m.insert(reqwest::header::RETRY_AFTER, v.parse().unwrap());
            m
        };
        assert_eq!(retry_after(&h("1")), Duration::from_millis(1000));
        assert_eq!(retry_after(&h("0.25")), Duration::from_millis(250));
        assert_eq!(retry_after(&h("60")), MAX_RETRY_AFTER);
        assert_eq!(retry_after(&h("soon")), DEFAULT_RETRY_AFTER);
        assert_eq!(
            retry_after(&reqwest::header::HeaderMap::new()),
            DEFAULT_RETRY_AFTER
        );
    }

    #[test]
    fn the_client_only_builds_info_price_urls() {
        for base in [
            "https://gateway.saxobank.com/openapi",
            "https://gateway.saxobank.com/openapi/",
            "http://127.0.0.1:1234",
        ] {
            let c = QuoteClient::new(reqwest::Client::new(), base);
            for uic in [0, 1, 4711, u64::MAX] {
                let url = c.infoprices_url(uic).unwrap();
                assert!(url.path().ends_with(INFOPRICES_PATH), "{url}");
                assert!(!url.path().contains("//"), "{url}");
                let pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
                assert_eq!(
                    pairs,
                    vec![
                        ("Uic".into(), uic.to_string()),
                        ("AssetType".into(), "Stock".into()),
                        ("FieldGroups".into(), FIELD_GROUPS.into()),
                    ]
                );
            }
        }
    }

    #[test]
    fn the_module_names_no_trading_or_account_endpoint() {
        let source = include_str!("quote.rs");
        let code = source
            .split("#[cfg(test)]")
            .next()
            .expect("split yields at least one part");
        let paths: Vec<&str> = code
            .match_indices("\"/")
            .map(|(i, _)| &code[i + 1..])
            .map(|rest| rest.split('"').next().unwrap_or_default())
            .collect();
        assert_eq!(paths, vec![INFOPRICES_PATH]);
        for word in [
            ["/ord", "ers"].concat(),
            ["/posi", "tions"].concat(),
            ["/acc", "ounts"].concat(),
            ["/p", "ort/"].concat(),
        ] {
            assert!(!code.contains(&word), "{word}");
        }
    }

    #[tokio::test]
    async fn fetches_with_the_given_token_and_parses_the_quote() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(INFOPRICES_PATH))
            .and(query_param("Uic", "4711"))
            .and(query_param("AssetType", "Stock"))
            .and(query_param("FieldGroups", FIELD_GROUPS))
            .and(header("Authorization", "Bearer stream-token"))
            .respond_with(ResponseTemplate::new(200).set_body_string(TRADABLE))
            .expect(1)
            .mount(&server)
            .await;
        let c = QuoteClient::new(reqwest::Client::new(), server.uri());
        let fetched = c.fetch(4711, "stream-token").await.unwrap();
        assert_eq!(fetched.quote.ask.unwrap().to_string(), "67.40");
    }

    #[tokio::test]
    async fn a_429_is_not_retried_and_reports_the_wait() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
            .expect(1)
            .mount(&server)
            .await;
        let c = QuoteClient::new(reqwest::Client::new(), server.uri());
        let started = Instant::now();
        let err = c.fetch(1, "t").await.unwrap_err();
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(
            matches!(err, QuoteError::RateLimited { retry_after } if retry_after == Duration::from_secs(1))
        );
        assert!(err.is_transient());
    }

    #[tokio::test]
    async fn other_statuses_keep_the_start_of_the_body() {
        let server = MockServer::start().await;
        let long = "é".repeat(400);
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401).set_body_string(long))
            .expect(1)
            .mount(&server)
            .await;
        let c = QuoteClient::new(reqwest::Client::new(), server.uri());
        match c.fetch(1, "t").await.unwrap_err() {
            e @ QuoteError::Status { .. } => {
                assert!(!e.is_transient());
                let QuoteError::Status { status, body } = e else {
                    unreachable!()
                };
                assert_eq!(status.as_u16(), 401);
                assert!(body.len() <= ERROR_BODY_BYTES && body.len() >= ERROR_BODY_BYTES - 1);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
