//! Live quotes from `saxo-stream`'s internal `GET /quote`. Read-only: this
//! module can only build that one path, and no order endpoint exists anywhere in
//! the analyst.

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Url;
use rust_decimal::Decimal;
use serde::Deserialize;
use thiserror::Error;

/// How long one quote request may take before it counts as failed.
pub const QUOTE_TIMEOUT: Duration = Duration::from_secs(3);

/// A quote as `saxo-stream` answers it. Anything Saxo omitted is `None`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Quote {
    /// Best bid, the price a sale receives.
    pub bid: Option<Decimal>,
    /// Best ask, the price a purchase pays.
    pub ask: Option<Decimal>,
    /// Saxo's `PriceTypeBid`, unchanged (`Tradable`, `Indicative`, ...).
    pub bid_type: Option<String>,
    /// Saxo's `PriceTypeAsk`, unchanged.
    pub ask_type: Option<String>,
    /// How many minutes Saxo says the quote is delayed.
    pub delayed_minutes: Option<i64>,
    /// The quote's currency.
    pub currency: Option<String>,
    /// Saxo's `LastUpdated`, as Saxo formatted it.
    pub saxo_last_updated: Option<String>,
}

impl Quote {
    /// The ask to buy at, when the quote is tradable, real time and positive.
    pub fn entry_ask(&self) -> Option<Decimal> {
        let ask = self.ask?;
        (self.ask_type.as_deref() == Some("Tradable")
            && self.delayed_minutes == Some(0)
            && ask > Decimal::ZERO)
            .then_some(ask)
    }

    /// The bid to sell at, when the quote is tradable, real time and positive.
    pub fn exit_bid(&self) -> Option<Decimal> {
        let bid = self.bid?;
        (self.bid_type.as_deref() == Some("Tradable")
            && self.delayed_minutes == Some(0)
            && bid > Decimal::ZERO)
            .then_some(bid)
    }
}

/// Why a quote could not be had.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum QuoteError {
    /// `saxo-stream` has no price to give right now (no token, rate limited);
    /// ask again after `retry_after`.
    #[error("quote unavailable, retry after {retry_after:?}")]
    Unavailable {
        /// What `saxo-stream` asked for.
        retry_after: Duration,
    },
    /// The pair is not registered or has no resolved instrument.
    #[error("no such instrument")]
    NotFound,
    /// Any other answer.
    #[error("quote endpoint answered {0}")]
    Status(u16),
    /// The request failed or timed out.
    #[error("quote request failed: {0}")]
    Transport(String),
    /// The body was not a quote.
    #[error("quote body unreadable: {0}")]
    Malformed(String),
}

impl QuoteError {
    /// The label of `analyst_quotes_total{outcome}`.
    pub fn outcome(&self) -> &'static str {
        match self {
            QuoteError::Unavailable { .. } => "unavailable",
            QuoteError::NotFound => "not_found",
            QuoteError::Status(_) => "error",
            QuoteError::Transport(_) => "transport",
            QuoteError::Malformed(_) => "malformed",
        }
    }
}

/// Where quotes come from.
#[async_trait]
pub trait QuoteSource: Send + Sync {
    /// The current quote of `ticker` on `mic`.
    async fn quote(&self, ticker: &str, mic: &str) -> Result<Quote, QuoteError>;
}

#[derive(Deserialize)]
struct Body {
    bid: Option<String>,
    ask: Option<String>,
    bid_type: Option<String>,
    ask_type: Option<String>,
    delayed_minutes: Option<i64>,
    currency: Option<String>,
    saxo_last_updated: Option<String>,
}

#[derive(Deserialize)]
struct Unavailable {
    retry_after_ms: Option<u64>,
}

fn decimal(text: Option<String>) -> Result<Option<Decimal>, QuoteError> {
    text.map(|t| Decimal::from_str(&t).map_err(|e| QuoteError::Malformed(format!("{t}: {e}"))))
        .transpose()
}

/// A [`QuoteSource`] over HTTP against `SAXO_QUOTES_URL`. Keeps connections
/// alive; it never retries (the caller owns the schedule).
#[derive(Debug, Clone)]
pub struct HttpQuoteSource {
    client: reqwest::Client,
    base: Url,
}

impl HttpQuoteSource {
    /// A client for the endpoint at `base`, like
    /// `http://saxo-quotes.chronicle.svc.cluster.local:8080`.
    pub fn new(base: &str) -> Result<HttpQuoteSource, String> {
        let base = Url::parse(base).map_err(|e| format!("SAXO_QUOTES_URL: {e}"))?;
        let client = reqwest::Client::builder()
            .timeout(QUOTE_TIMEOUT)
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(HttpQuoteSource { client, base })
    }

    /// The only URL this client ever requests: `<base>/quote?ticker=&mic=`.
    pub fn quote_url(&self, ticker: &str, mic: &str) -> Url {
        let mut url = self.base.clone();
        url.set_path("/quote");
        url.set_query(None);
        url.query_pairs_mut()
            .append_pair("ticker", ticker)
            .append_pair("mic", mic);
        url
    }
}

#[async_trait]
impl QuoteSource for HttpQuoteSource {
    async fn quote(&self, ticker: &str, mic: &str) -> Result<Quote, QuoteError> {
        let response = self
            .client
            .get(self.quote_url(ticker, mic))
            .send()
            .await
            .map_err(|e| QuoteError::Transport(e.to_string()))?;
        let status = response.status();
        if status.as_u16() == 503 {
            let retry_after_ms = response
                .json::<Unavailable>()
                .await
                .ok()
                .and_then(|u| u.retry_after_ms)
                .unwrap_or(1000);
            return Err(QuoteError::Unavailable {
                retry_after: Duration::from_millis(retry_after_ms),
            });
        }
        if status.as_u16() == 404 {
            return Err(QuoteError::NotFound);
        }
        if !status.is_success() {
            return Err(QuoteError::Status(status.as_u16()));
        }
        let body: Body = response
            .json()
            .await
            .map_err(|e| QuoteError::Malformed(e.to_string()))?;
        Ok(Quote {
            bid: decimal(body.bid)?,
            ask: decimal(body.ask)?,
            bid_type: body.bid_type,
            ask_type: body.ask_type,
            delayed_minutes: body.delayed_minutes,
            currency: body.currency,
            saxo_last_updated: body.saxo_last_updated,
        })
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn body(ask: &str, ask_type: &str, delayed: i64) -> serde_json::Value {
        serde_json::json!({
            "ticker": "AAA", "mic": "XSTO", "uic": 1,
            "bid": "99.50", "ask": ask, "bid_size": null, "ask_size": null,
            "bid_type": "Tradable", "ask_type": ask_type, "market_state": "Open",
            "delayed_minutes": delayed, "currency": "SEK",
            "saxo_last_updated": "2026-11-10T08:00:00.123Z",
            "fetched_at": "2026-11-10T08:00:00.200Z", "saxo_ms": 40
        })
    }

    #[tokio::test]
    async fn the_client_only_ever_requests_get_quote() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/quote"))
            .and(query_param("ticker", "AAA"))
            .and(query_param("mic", "XSTO"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body("100.00", "Tradable", 0)))
            .mount(&server)
            .await;
        let source = HttpQuoteSource::new(&server.uri()).expect("client");
        let quote = source.quote("AAA", "XSTO").await.expect("quote");
        assert_eq!(
            quote.entry_ask(),
            Some(Decimal::from_str("100.00").expect("decimal"))
        );
        assert_eq!(quote.currency.as_deref(), Some("SEK"));
        let requests = server.received_requests().await.expect("recorded");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method.as_str(), "GET");
        assert_eq!(requests[0].url.path(), "/quote");
        let url = source.quote_url("A B", "XSTO");
        assert_eq!(url.path(), "/quote");
        assert_eq!(url.query(), Some("ticker=A+B&mic=XSTO"));
    }

    #[tokio::test]
    async fn a_base_url_with_a_path_still_gives_only_the_quote_path() {
        let source = HttpQuoteSource::new("http://saxo-quotes:8080/anything?x=1").expect("client");
        let url = source.quote_url("AAA", "XSTO");
        assert_eq!(
            url.as_str(),
            "http://saxo-quotes:8080/quote?ticker=AAA&mic=XSTO"
        );
    }

    #[tokio::test]
    async fn a_503_gives_the_wait_it_asked_for() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(503).set_body_json(
                    serde_json::json!({"error": "no_token", "retry_after_ms": 1500}),
                ),
            )
            .mount(&server)
            .await;
        let source = HttpQuoteSource::new(&server.uri()).expect("client");
        assert_eq!(
            source.quote("AAA", "XSTO").await,
            Err(QuoteError::Unavailable {
                retry_after: Duration::from_millis(1500)
            })
        );
    }

    #[tokio::test]
    async fn not_found_other_statuses_and_garbage_are_told_apart() {
        for (status, expected) in [(404, QuoteError::NotFound), (502, QuoteError::Status(502))] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;
            let source = HttpQuoteSource::new(&server.uri()).expect("client");
            assert_eq!(source.quote("AAA", "XSTO").await, Err(expected));
        }
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"bid": "x"})))
            .mount(&server)
            .await;
        let source = HttpQuoteSource::new(&server.uri()).expect("client");
        assert!(matches!(
            source.quote("AAA", "XSTO").await,
            Err(QuoteError::Malformed(_))
        ));
    }

    #[test]
    fn only_a_tradable_undelayed_positive_price_is_accepted() {
        let quote = |ask: &str, kind: &str, delayed: Option<i64>| Quote {
            ask: Some(Decimal::from_str(ask).expect("decimal")),
            ask_type: Some(kind.to_owned()),
            delayed_minutes: delayed,
            ..Quote::default()
        };
        assert!(quote("10", "Tradable", Some(0)).entry_ask().is_some());
        assert!(quote("10", "Indicative", Some(0)).entry_ask().is_none());
        assert!(quote("10", "Tradable", Some(5)).entry_ask().is_none());
        assert!(quote("10", "Tradable", None).entry_ask().is_none());
        assert!(quote("0", "Tradable", Some(0)).entry_ask().is_none());
        let bid = Quote {
            bid: Some(Decimal::ONE),
            bid_type: Some("Tradable".into()),
            delayed_minutes: Some(0),
            ..Quote::default()
        };
        assert_eq!(bid.exit_bid(), Some(Decimal::ONE));
        assert!(Quote {
            bid_type: Some("Indicative".into()),
            ..bid
        }
        .exit_bid()
        .is_none());
    }
}
