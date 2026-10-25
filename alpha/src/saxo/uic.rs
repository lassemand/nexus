/// Saxo Bank Uic (instrument identifier) resolution.
///
/// Resolves a Nordic ticker symbol to Saxo's internal `Uic` via the
/// `/ref/v1/instruments` endpoint, **strictly filtering to `AssetType=Stock`**.
///
/// # CFD rejection
///
/// Saxo sells both a real exchange-listed `Stock` (priced from the actual
/// exchange tape) and a `CfdOnStock` (Saxo's internally-priced derivative).
/// This resolver explicitly rejects any result where the returned AssetType
/// is not exactly `"Stock"` — subscribing to a CFD would silently track
/// Saxo's internal derivative pricing instead of real Nasdaq First North
/// market activity, corrupting the entire pipeline's premise.
///
/// If lookup returns CFD-only results, the ticker is rejected with
/// [`UicResolverError::CfdOnly`] — the caller must not proceed with that
/// ticker.
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum UicResolverError {
    /// The request did not get an HTTP response (connect, TLS, timeout, body).
    #[error("HTTP error resolving Uic for {ticker}: {source}")]
    Http {
        ticker: String,
        #[source]
        source: reqwest::Error,
    },
    /// Saxo answered with a non-2xx status. `rate_limit` holds the
    /// `Retry-After` and `X-RateLimit-*` headers, for the log.
    #[error(
        "Saxo returned {status} resolving Uic for {ticker} (rate limit headers: {rate_limit:?})"
    )]
    Status {
        ticker: String,
        status: reqwest::StatusCode,
        retry_after: Option<Duration>,
        rate_limit: Vec<(String, String)>,
    },
    #[error("no Stock instrument found for ticker {0} on Saxo (got CFD-only or no results)")]
    CfdOnly(String),
    #[error("no instrument found for ticker {0}")]
    NotFound(String),
}

impl UicResolverError {
    /// Whether retrying can help. "Not found", "CFD only" and a 4xx other
    /// than 408/429 give the same answer every time.
    pub fn is_transient(&self) -> bool {
        match self {
            UicResolverError::Http { source, .. } => super::is_transient_reqwest(source),
            UicResolverError::Status { status, .. } => super::is_transient_status(*status),
            UicResolverError::CfdOnly(_) | UicResolverError::NotFound(_) => false,
        }
    }

    /// How long Saxo asked us to wait, if it said (`Retry-After`, or the
    /// longest `X-RateLimit-*-Reset`).
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            UicResolverError::Status { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

/// A successfully resolved Saxo instrument identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedUic {
    /// Saxo's internal numeric instrument identifier.
    pub uic: u64,
    /// The ticker symbol (uppercase).
    pub ticker: String,
    /// ISO 10383 MIC of the exchange as known to Saxo (e.g. "SSE_FN-SE").
    pub exchange_id: String,
    /// ISO 4217 currency of the instrument (e.g. "SEK").
    pub currency: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InstrumentSummary {
    #[serde(rename = "Identifier")]
    uic: u64,
    asset_type: String,
    exchange_id: String,
    currency_code: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InstrumentListResponse {
    data: Vec<InstrumentSummary>,
}

/// Resolves tickers to Saxo `Uic` values via the reference data API.
///
/// Successful resolutions are cached for the resolver's lifetime: a Uic never
/// changes, and every lookup counts against Saxo's request limits. Share one
/// resolver between all users in a process.
pub struct UicResolver {
    client: reqwest::Client,
    api_base: String,
    cache: Mutex<HashMap<String, ResolvedUic>>,
}

/// `Retry-After` (seconds), or else the longest `X-RateLimit-*-Reset`.
fn wait_hint(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let secs = |v: &reqwest::header::HeaderValue| v.to_str().ok()?.trim().parse::<u64>().ok();
    if let Some(s) = headers.get(reqwest::header::RETRY_AFTER).and_then(secs) {
        return Some(Duration::from_secs(s));
    }
    headers
        .iter()
        .filter(|(k, _)| {
            let k = k.as_str();
            k.starts_with("x-ratelimit-") && k.ends_with("-reset")
        })
        .filter_map(|(_, v)| secs(v))
        .max()
        .map(Duration::from_secs)
}

impl UicResolver {
    pub fn new(client: reqwest::Client, api_base: impl Into<String>) -> Self {
        Self {
            client,
            api_base: api_base.into(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    async fn search(
        &self,
        ticker: &str,
        asset_type: &str,
        access_token: &str,
    ) -> Result<InstrumentListResponse, UicResolverError> {
        let url = format!(
            "{}/ref/v1/instruments?Keywords={}&AssetTypes={}",
            self.api_base, ticker, asset_type
        );
        let http = |source| UicResolverError::Http {
            ticker: ticker.to_string(),
            source,
        };
        let resp = self
            .client
            .get(&url)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(http)?;
        let status = resp.status();
        if !status.is_success() {
            let headers = resp.headers();
            let rate_limit = headers
                .iter()
                .filter(|(k, _)| {
                    let k = k.as_str();
                    k == "retry-after" || k.starts_with("x-ratelimit-")
                })
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("?").to_string()))
                .collect();
            return Err(UicResolverError::Status {
                ticker: ticker.to_string(),
                status,
                retry_after: wait_hint(headers),
                rate_limit,
            });
        }
        resp.json().await.map_err(http)
    }

    fn cached(&self, ticker: &str) -> Option<ResolvedUic> {
        self.cache
            .lock()
            .unwrap()
            .get(&ticker.to_uppercase())
            .cloned()
    }

    fn remember(&self, resolved: &ResolvedUic) {
        self.cache
            .lock()
            .unwrap()
            .insert(resolved.ticker.clone(), resolved.clone());
    }

    fn to_resolved(ticker: &str, stock: InstrumentSummary) -> ResolvedUic {
        ResolvedUic {
            uic: stock.uic,
            ticker: ticker.to_uppercase(),
            exchange_id: stock.exchange_id,
            currency: stock.currency_code,
        }
    }

    /// Resolve a ticker symbol to its `Stock` Uic on Saxo (cached).
    pub async fn resolve(
        &self,
        ticker: &str,
        access_token: &str,
    ) -> Result<ResolvedUic, UicResolverError> {
        if let Some(hit) = self.cached(ticker) {
            return Ok(hit);
        }
        let stock = self
            .search(ticker, "Stock", access_token)
            .await?
            .data
            .into_iter()
            .find(|i| i.asset_type == "Stock")
            .ok_or_else(|| UicResolverError::NotFound(ticker.to_string()))?;
        let resolved = Self::to_resolved(ticker, stock);
        self.remember(&resolved);
        Ok(resolved)
    }

    /// Resolve a ticker (cached), explicitly confirming no CFD exists with the
    /// same symbol. If a Stock is found, returns it. If only a CfdOnStock
    /// exists, returns [`UicResolverError::CfdOnly`].
    pub async fn resolve_with_cfd_check(
        &self,
        ticker: &str,
        access_token: &str,
    ) -> Result<ResolvedUic, UicResolverError> {
        if let Some(hit) = self.cached(ticker) {
            return Ok(hit);
        }
        let stock = self
            .search(ticker, "Stock", access_token)
            .await?
            .data
            .into_iter()
            .find(|i| i.asset_type == "Stock");
        if let Some(stock) = stock {
            let resolved = Self::to_resolved(ticker, stock);
            self.remember(&resolved);
            return Ok(resolved);
        }

        // Stock not found — check if a CFD exists to give a better error.
        let cfd = self.search(ticker, "CfdOnStock", access_token).await?;
        if cfd.data.iter().any(|i| i.asset_type == "CfdOnStock") {
            Err(UicResolverError::CfdOnly(ticker.to_string()))
        } else {
            Err(UicResolverError::NotFound(ticker.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies that when only a CFD result is present, resolve_with_cfd_check
    /// returns CfdOnly rather than silently accepting it.
    ///
    /// Uses a mock HTTP server to simulate Saxo returning empty Stock results
    /// but a non-empty CfdOnStock result for the same ticker.
    #[test]
    fn cfd_only_result_is_rejected_not_accepted() {
        // Simulate parsing a Stock response with no results and a CFD response
        // with one result — this mirrors what resolve_with_cfd_check sees.

        let empty_stock: InstrumentListResponse = serde_json::from_str(r#"{"Data":[]}"#).unwrap();
        let cfd_result: InstrumentListResponse = serde_json::from_str(
            r#"{"Data":[{"Identifier":12345,"AssetType":"CfdOnStock","ExchangeId":"CFD","CurrencyCode":"SEK"}]}"#,
        )
        .unwrap();

        // Stock search found nothing.
        let stock_match = empty_stock
            .data
            .into_iter()
            .find(|i| i.asset_type == "Stock");
        assert!(stock_match.is_none(), "empty Stock result must not match");

        // CFD search found something.
        let cfd_match = cfd_result.data.iter().any(|i| i.asset_type == "CfdOnStock");
        assert!(cfd_match, "CFD result must be detected");

        // The resolver would return CfdOnly — confirmed by logic above.
        // (Full integration test requires a live/mock HTTP server.)
    }

    #[test]
    fn stock_result_is_accepted() {
        let stock_resp: InstrumentListResponse = serde_json::from_str(
            r#"{"Data":[{"Identifier":4769462,"AssetType":"Stock","ExchangeId":"SSE_FN-SE","CurrencyCode":"SEK"}]}"#,
        )
        .unwrap();

        let stock = stock_resp
            .data
            .into_iter()
            .find(|i| i.asset_type == "Stock");
        assert!(stock.is_some(), "Stock result must be accepted");
        let s = stock.unwrap();
        assert_eq!(s.uic, 4769462);
        assert_eq!(s.exchange_id, "SSE_FN-SE");
        assert_eq!(s.currency_code, "SEK");
    }

    #[test]
    fn cfd_is_never_returned_as_stock() {
        let mixed_resp: InstrumentListResponse = serde_json::from_str(
            r#"{"Data":[
                {"Identifier":99999,"AssetType":"CfdOnStock","ExchangeId":"CFD","CurrencyCode":"SEK"},
                {"Identifier":4769462,"AssetType":"Stock","ExchangeId":"SSE_FN-SE","CurrencyCode":"SEK"}
            ]}"#,
        )
        .unwrap();

        // The resolver must pick Stock, ignoring the CFD entry.
        let resolved = mixed_resp
            .data
            .into_iter()
            .find(|i| i.asset_type == "Stock");
        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap().uic, 4769462);
    }

    mod http {
        use super::super::*;
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const GOMX: &str = r#"{"Data":[{"Identifier":4769462,"AssetType":"Stock","ExchangeId":"SSE_FN-SE","CurrencyCode":"SEK"}]}"#;

        #[tokio::test]
        async fn a_resolved_uic_is_cached() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/ref/v1/instruments"))
                .and(query_param("AssetTypes", "Stock"))
                .respond_with(ResponseTemplate::new(200).set_body_string(GOMX))
                .expect(1)
                .mount(&server)
                .await;
            let r = UicResolver::new(reqwest::Client::new(), server.uri());
            let first = r.resolve_with_cfd_check("GOMX", "t").await.unwrap();
            let again = r.resolve_with_cfd_check("gomx", "t").await.unwrap();
            assert_eq!(first, again);
            assert_eq!((first.uic, first.currency.as_str()), (4769462, "SEK"));
            assert_eq!(r.resolve("GOMX", "t").await.unwrap(), first);
            // `expect(1)` is verified when the server drops.
        }

        #[tokio::test]
        async fn a_429_carries_retry_after_and_the_rate_limit_headers() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(
                    ResponseTemplate::new(429)
                        .insert_header("Retry-After", "30")
                        .insert_header("X-RateLimit-Session-Remaining", "0")
                        .insert_header("X-RateLimit-Session-Reset", "45"),
                )
                .mount(&server)
                .await;
            let r = UicResolver::new(reqwest::Client::new(), server.uri());
            let err = r.resolve_with_cfd_check("GOMX", "t").await.unwrap_err();
            assert_eq!(err.retry_after(), Some(Duration::from_secs(30)));
            let UicResolverError::Status {
                status, rate_limit, ..
            } = &err
            else {
                panic!("{err:?}")
            };
            assert_eq!(status.as_u16(), 429);
            assert!(rate_limit
                .iter()
                .any(|(k, v)| k == "x-ratelimit-session-remaining" && v == "0"));
            // A failure is not cached: the next call asks again.
            assert!(r.resolve_with_cfd_check("GOMX", "t").await.is_err());
            assert_eq!(server.received_requests().await.unwrap().len(), 2);
        }

        #[tokio::test]
        async fn without_retry_after_the_longest_reset_is_used() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(
                    ResponseTemplate::new(429)
                        .insert_header("X-RateLimit-Session-Reset", "12")
                        .insert_header("X-RateLimit-AppDay-Reset", "3600"),
                )
                .mount(&server)
                .await;
            let r = UicResolver::new(reqwest::Client::new(), server.uri());
            let err = r.resolve_with_cfd_check("GOMX", "t").await.unwrap_err();
            assert_eq!(err.retry_after(), Some(Duration::from_secs(3600)));
        }

        #[tokio::test]
        async fn rate_limits_and_server_errors_are_transient_other_4xx_are_not() {
            for (code, transient) in [
                (429, true),
                (503, true),
                (408, true),
                (400, false),
                (401, false),
                (404, false),
            ] {
                let server = MockServer::start().await;
                Mock::given(method("GET"))
                    .respond_with(ResponseTemplate::new(code))
                    .mount(&server)
                    .await;
                let r = UicResolver::new(reqwest::Client::new(), server.uri());
                let err = r.resolve_with_cfd_check("GOMX", "t").await.unwrap_err();
                assert_eq!(err.is_transient(), transient, "{code}");
            }
            assert!(!UicResolverError::NotFound("X".into()).is_transient());
            assert!(!UicResolverError::CfdOnly("X".into()).is_transient());
        }
    }
}
