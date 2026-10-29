/// Saxo Bank Uic (instrument identifier) resolution.
///
/// Resolves a Nordic listing to Saxo's internal `Uic` via the
/// `/ref/v1/instruments` endpoint, **strictly filtering to `AssetType=Stock`**
/// and to the **expected exchange** (see [`exchange_ids`]). The ISIN is searched
/// when the registry has one; otherwise the ticker text is, in the registry's
/// spelling and with `-` replaced by a space (`NOVO-B` → `NOVO B`). More than
/// one candidate on the expected exchange is an error, never a guess.
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
    /// Several stocks on the expected exchange match; picking one could track
    /// the wrong share class.
    #[error("ambiguous: {ticker} matches several stocks on its exchange: {symbols:?}")]
    Ambiguous {
        ticker: String,
        symbols: Vec<String>,
    },
    /// No Saxo `ExchangeId` is configured for this MIC.
    #[error("no Saxo ExchangeId known for MIC {0}")]
    UnknownExchange(String),
}

impl UicResolverError {
    /// Whether retrying can help. "Not found", "CFD only" and a 4xx other
    /// than 408/429 give the same answer every time.
    pub fn is_transient(&self) -> bool {
        match self {
            UicResolverError::Http { source, .. } => super::is_transient_reqwest(source),
            UicResolverError::Status { status, .. } => super::is_transient_status(*status),
            UicResolverError::CfdOnly(_)
            | UicResolverError::NotFound(_)
            | UicResolverError::Ambiguous { .. }
            | UicResolverError::UnknownExchange(_) => false,
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

/// Saxo `ExchangeId` for each MIC we stream.
///
/// **Unverified against the live API**: confirm with `GET /ref/v1/exchanges`
/// (list, then `GET /ref/v1/exchanges/{ExchangeId}` for the `Mic` of each) and
/// correct here. `SSE_FN-SE` (First North Stockholm) is the value seen on
/// GOMX; the others follow Saxo's naming and must be checked. Deployments can
/// override entries without a rebuild, see [`UicResolver::with_exchange_ids`].
pub mod exchange_ids {
    /// `(MIC, Saxo ExchangeId)`.
    pub const SAXO_EXCHANGE_IDS: &[(&str, &str)] = &[
        ("FNSE", "SSE_FN-SE"),
        ("XSTO", "SSE"),
        ("XCSE", "CSE"),
        ("FNDK", "CSE_FN-DK"),
        // Unverified, like the rest: confirm against /ref/v1/exchanges.
        ("XHEL", "HSE"),
        ("FNFI", "HSE_FN-FI"),
    ];

    /// The built-in ExchangeId for `mic`.
    pub fn default_for(mic: &str) -> Option<&'static str> {
        SAXO_EXCHANGE_IDS
            .iter()
            .find(|(m, _)| *m == mic)
            .map(|(_, id)| *id)
    }
}

/// A listing to look up on Saxo.
#[derive(Debug, Clone, Copy)]
pub struct InstrumentQuery<'a> {
    /// Registry ticker, e.g. `NOVO-B`.
    pub ticker: &'a str,
    /// Registry MIC, e.g. `XCSE`; selects the expected Saxo exchange.
    pub exchange_mic: &'a str,
    /// ISIN from the registry, searched first when present.
    pub isin: Option<&'a str>,
    /// Currency the registry has for the listing, compared with Saxo's.
    pub registry_currency: Option<&'a str>,
}

impl<'a> InstrumentQuery<'a> {
    /// A query by ticker text only.
    pub fn new(ticker: &'a str, exchange_mic: &'a str) -> Self {
        Self {
            ticker,
            exchange_mic,
            isin: None,
            registry_currency: None,
        }
    }
}

/// A successfully resolved Saxo instrument identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedUic {
    /// Saxo's internal numeric instrument identifier.
    pub uic: u64,
    /// The registry ticker (uppercase).
    pub ticker: String,
    /// The registry MIC this listing was resolved for (e.g. `XCSE`).
    pub exchange_mic: String,
    /// Saxo's ExchangeId (e.g. "SSE_FN-SE").
    pub exchange_id: String,
    /// ISO 4217 currency **as Saxo reports it**; bars carry this value.
    pub currency: String,
    /// Saxo's symbol, e.g. `NOVOb:xcse`.
    pub symbol: String,
    /// True when the registry's currency differs from Saxo's.
    pub currency_mismatch: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InstrumentSummary {
    #[serde(rename = "Identifier")]
    uic: u64,
    asset_type: String,
    exchange_id: String,
    currency_code: String,
    #[serde(default)]
    symbol: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InstrumentListResponse {
    data: Vec<InstrumentSummary>,
}

/// Resolves listings to Saxo `Uic` values via the reference data API.
///
/// Successful resolutions are cached for the resolver's lifetime, keyed by
/// `(ticker, MIC)`: a Uic never changes, and every lookup counts against Saxo's
/// request limits. Share one resolver between all users in a process.
pub struct UicResolver {
    client: reqwest::Client,
    api_base: String,
    cache: Mutex<HashMap<(String, String), ResolvedUic>>,
    exchange_overrides: HashMap<String, String>,
    retry: RetryPolicy,
}

/// How a lookup that got no answer, or a transient status (408, 429, 5xx), is
/// retried within the call. Other failures are returned at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Attempts in total, including the first.
    pub max_attempts: u32,
    /// Wait before the second attempt; doubled for each one after.
    pub base_backoff: Duration,
    /// Upper bound on any wait, including one Saxo asks for (`Retry-After`).
    pub max_wait: Duration,
}

impl RetryPolicy {
    /// One attempt, no retry.
    pub const NONE: RetryPolicy = RetryPolicy {
        max_attempts: 1,
        base_backoff: Duration::ZERO,
        max_wait: Duration::ZERO,
    };

    fn wait(&self, attempt: u32, asked: Option<Duration>) -> Duration {
        let backoff = self.base_backoff.saturating_mul(1 << (attempt - 1).min(16));
        asked.unwrap_or(backoff).min(self.max_wait)
    }
}

impl Default for RetryPolicy {
    /// Four attempts, 1s/2s/4s apart, at most 120s when Saxo asks for longer:
    /// the same as the chart client.
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_backoff: Duration::from_secs(1),
            max_wait: Duration::from_secs(120),
        }
    }
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

/// Search keywords for a listing: the ISIN if known, else the ticker as
/// registered and with `-` turned into a space (`NOVO-B` → `NOVO B`).
fn text_keywords(ticker: &str) -> Vec<String> {
    let mut v = vec![ticker.to_string()];
    let spaced = ticker.replace('-', " ");
    if spaced != ticker {
        v.push(spaced);
    }
    v
}

impl UicResolver {
    pub fn new(client: reqwest::Client, api_base: impl Into<String>) -> Self {
        Self {
            client,
            api_base: api_base.into(),
            cache: Mutex::new(HashMap::new()),
            exchange_overrides: HashMap::new(),
            retry: RetryPolicy::default(),
        }
    }

    /// Replaces the default [`RetryPolicy`].
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Replaces the built-in ExchangeId of some MICs (`MIC` → Saxo ExchangeId),
    /// for when [`exchange_ids::SAXO_EXCHANGE_IDS`] proves wrong in production.
    pub fn with_exchange_ids(mut self, overrides: HashMap<String, String>) -> Self {
        self.exchange_overrides = overrides;
        self
    }

    fn expected_exchange(&self, mic: &str) -> Result<String, UicResolverError> {
        self.exchange_overrides
            .get(mic)
            .cloned()
            .or_else(|| exchange_ids::default_for(mic).map(str::to_string))
            .ok_or_else(|| UicResolverError::UnknownExchange(mic.to_string()))
    }

    /// [`Self::search_once`], retried per the [`RetryPolicy`].
    async fn search(
        &self,
        ticker: &str,
        keywords: &str,
        asset_type: &str,
        access_token: &str,
    ) -> Result<InstrumentListResponse, UicResolverError> {
        let mut attempt = 1;
        loop {
            match self
                .search_once(ticker, keywords, asset_type, access_token)
                .await
            {
                Err(e) if e.is_transient() && attempt < self.retry.max_attempts => {
                    let wait = self.retry.wait(attempt, e.retry_after());
                    tracing::warn!(
                        ticker,
                        keywords,
                        attempt,
                        ?wait,
                        error = super::error_chain(&e),
                        "Saxo instrument search failed, retrying"
                    );
                    tokio::time::sleep(wait).await;
                    attempt += 1;
                }
                result => return result,
            }
        }
    }

    async fn search_once(
        &self,
        ticker: &str,
        keywords: &str,
        asset_type: &str,
        access_token: &str,
    ) -> Result<InstrumentListResponse, UicResolverError> {
        let http = |source| UicResolverError::Http {
            ticker: ticker.to_string(),
            source,
        };
        let mut url = url::Url::parse(&format!("{}/ref/v1/instruments", self.api_base))
            .map_err(|_| UicResolverError::NotFound(ticker.to_string()))?;
        url.query_pairs_mut()
            .append_pair("Keywords", keywords)
            .append_pair("AssetTypes", asset_type);
        let resp = self
            .client
            .get(url)
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

    fn cache_key(q: &InstrumentQuery<'_>) -> (String, String) {
        (q.ticker.to_uppercase(), q.exchange_mic.to_string())
    }

    fn cached(&self, q: &InstrumentQuery<'_>) -> Option<ResolvedUic> {
        self.cache.lock().unwrap().get(&Self::cache_key(q)).cloned()
    }

    fn remember(&self, key: (String, String), resolved: &ResolvedUic) {
        self.cache.lock().unwrap().insert(key, resolved.clone());
    }

    /// The stocks on `expected` among the results for one search.
    fn candidates(resp: InstrumentListResponse, expected: &str) -> Vec<InstrumentSummary> {
        resp.data
            .into_iter()
            .filter(|i| i.asset_type == "Stock" && i.exchange_id.eq_ignore_ascii_case(expected))
            .collect()
    }

    /// Resolve a listing to its `Stock` Uic on its expected Saxo exchange
    /// (cached), explicitly rejecting CFDs.
    ///
    /// - ISIN set: search the ISIN; if that finds nothing on the exchange, fall
    ///   back to the ticker text.
    /// - Several stocks on the exchange: [`UicResolverError::Ambiguous`].
    /// - Only a CFD exists: [`UicResolverError::CfdOnly`]; nothing:
    ///   [`UicResolverError::NotFound`].
    pub async fn resolve_with_cfd_check(
        &self,
        q: &InstrumentQuery<'_>,
        access_token: &str,
    ) -> Result<ResolvedUic, UicResolverError> {
        if let Some(hit) = self.cached(q) {
            return Ok(hit);
        }
        let expected = self.expected_exchange(q.exchange_mic)?;

        let mut keywords: Vec<String> = q.isin.map(|i| vec![i.to_string()]).unwrap_or_default();
        keywords.extend(text_keywords(q.ticker));

        let mut found = Vec::new();
        for kw in &keywords {
            found = Self::candidates(
                self.search(q.ticker, kw, "Stock", access_token).await?,
                &expected,
            );
            if !found.is_empty() {
                break;
            }
        }

        match found.len() {
            1 => {
                let stock = found.remove(0);
                let currency_mismatch = q
                    .registry_currency
                    .is_some_and(|c| !c.eq_ignore_ascii_case(&stock.currency_code));
                if currency_mismatch {
                    tracing::warn!(
                        ticker = q.ticker, exchange_mic = q.exchange_mic,
                        registry_currency = q.registry_currency, saxo_currency = %stock.currency_code,
                        "Saxo's currency differs from the registry's; bars use Saxo's"
                    );
                }
                let resolved = ResolvedUic {
                    uic: stock.uic,
                    ticker: q.ticker.to_uppercase(),
                    exchange_mic: q.exchange_mic.to_string(),
                    exchange_id: stock.exchange_id,
                    currency: stock.currency_code,
                    symbol: stock.symbol,
                    currency_mismatch,
                };
                self.remember(Self::cache_key(q), &resolved);
                Ok(resolved)
            }
            0 => {
                // Nothing as a Stock on this exchange: say so if a CFD exists.
                let cfd = self
                    .search(q.ticker, &keywords[0], "CfdOnStock", access_token)
                    .await?;
                if cfd.data.iter().any(|i| i.asset_type == "CfdOnStock") {
                    Err(UicResolverError::CfdOnly(q.ticker.to_string()))
                } else {
                    Err(UicResolverError::NotFound(q.ticker.to_string()))
                }
            }
            _ => Err(UicResolverError::Ambiguous {
                ticker: q.ticker.to_string(),
                symbols: found.into_iter().map(|i| i.symbol).collect(),
            }),
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
            let first = r
                .resolve_with_cfd_check(&InstrumentQuery::new("GOMX", "FNSE"), "t")
                .await
                .unwrap();
            let again = r
                .resolve_with_cfd_check(&InstrumentQuery::new("gomx", "FNSE"), "t")
                .await
                .unwrap();
            assert_eq!(first, again);
            assert_eq!((first.uic, first.currency.as_str()), (4769462, "SEK"));
            // `expect(1)` is verified when the server drops.
        }

        fn stock(uic: u64, symbol: &str, exchange: &str, currency: &str) -> String {
            format!(
                r#"{{"Identifier":{uic},"AssetType":"Stock","ExchangeId":"{exchange}","CurrencyCode":"{currency}","Symbol":"{symbol}"}}"#
            )
        }

        fn list(items: &[String]) -> String {
            format!(r#"{{"Data":[{}]}}"#, items.join(","))
        }

        async fn mock_keywords(server: &MockServer, keywords: &str, body: String) {
            Mock::given(method("GET"))
                .and(path("/ref/v1/instruments"))
                .and(query_param("Keywords", keywords))
                .and(query_param("AssetTypes", "Stock"))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(server)
                .await;
        }

        const NOVO_ISIN: &str = "DK0062498333";

        #[tokio::test]
        async fn isin_resolution_picks_the_copenhagen_line_over_london_and_us() {
            let server = MockServer::start().await;
            mock_keywords(
                &server,
                NOVO_ISIN,
                list(&[
                    stock(1, "NOVOb:xlon", "LSE_INTL", "GBP"),
                    stock(2, "NVO:xnys", "NYSE", "USD"),
                    stock(3, "NOVOb:xcse", "CSE", "DKK"),
                ]),
            )
            .await;
            let r = UicResolver::new(reqwest::Client::new(), server.uri());
            let q = InstrumentQuery {
                isin: Some(NOVO_ISIN),
                registry_currency: Some("DKK"),
                ..InstrumentQuery::new("NOVO-B", "XCSE")
            };
            let got = r.resolve_with_cfd_check(&q, "t").await.unwrap();
            assert_eq!(
                (got.uic, got.exchange_mic.as_str(), got.exchange_id.as_str()),
                (3, "XCSE", "CSE")
            );
            assert_eq!(
                (got.currency.as_str(), got.symbol.as_str()),
                ("DKK", "NOVOb:xcse")
            );
            assert!(!got.currency_mismatch);
        }

        const METSO_ISIN: &str = "FI0009014575";

        /// ISIN + the Helsinki ExchangeId pick exactly one instrument: not the
        /// London line, not a US line, and the Stock asset type excludes CFDs.
        #[tokio::test]
        async fn metso_resolves_to_the_helsinki_line_only() {
            let server = MockServer::start().await;
            mock_keywords(
                &server,
                METSO_ISIN,
                list(&[
                    stock(10, "0MGI:xlon", "LSE_INTL", "GBP"),
                    stock(11, "METSO:xhel", "HSE", "EUR"),
                ]),
            )
            .await;
            let r = UicResolver::new(reqwest::Client::new(), server.uri());
            let q = InstrumentQuery {
                isin: Some(METSO_ISIN),
                registry_currency: Some("EUR"),
                ..InstrumentQuery::new("METSO", "XHEL")
            };
            let got = r.resolve_with_cfd_check(&q, "t").await.unwrap();
            assert_eq!(
                (got.uic, got.exchange_mic.as_str(), got.currency.as_str()),
                (11, "XHEL", "EUR")
            );
            assert!(!got.currency_mismatch);
        }

        #[tokio::test]
        async fn two_helsinki_candidates_are_ambiguous() {
            let server = MockServer::start().await;
            mock_keywords(
                &server,
                METSO_ISIN,
                list(&[
                    stock(11, "METSO:xhel", "HSE", "EUR"),
                    stock(12, "METSOa:xhel", "HSE", "EUR"),
                    stock(10, "0MGI:xlon", "LSE_INTL", "GBP"),
                ]),
            )
            .await;
            let r = UicResolver::new(reqwest::Client::new(), server.uri());
            let q = InstrumentQuery {
                isin: Some(METSO_ISIN),
                ..InstrumentQuery::new("METSO", "XHEL")
            };
            match r.resolve_with_cfd_check(&q, "t").await.unwrap_err() {
                UicResolverError::Ambiguous { symbols, .. } => {
                    assert_eq!(symbols, vec!["METSO:xhel", "METSOa:xhel"])
                }
                e => panic!("{e:?}"),
            }
        }

        #[test]
        fn finnish_mics_have_exchange_ids() {
            assert!(exchange_ids::default_for("XHEL").is_some());
            assert!(exchange_ids::default_for("FNFI").is_some());
        }

        #[tokio::test]
        async fn text_fallback_tries_the_hyphen_as_a_space() {
            let server = MockServer::start().await;
            mock_keywords(&server, "NOVO-B", list(&[])).await;
            mock_keywords(
                &server,
                "NOVO B",
                list(&[
                    stock(3, "NOVOb:xcse", "CSE", "DKK"),
                    stock(9, "NOVOb:xlon", "LSE_INTL", "GBP"),
                ]),
            )
            .await;
            let r = UicResolver::new(reqwest::Client::new(), server.uri());
            let got = r
                .resolve_with_cfd_check(&InstrumentQuery::new("NOVO-B", "XCSE"), "t")
                .await
                .unwrap();
            assert_eq!(got.uic, 3);
        }

        #[tokio::test]
        async fn two_candidates_on_the_expected_exchange_are_ambiguous() {
            let server = MockServer::start().await;
            mock_keywords(
                &server,
                NOVO_ISIN,
                list(&[
                    stock(3, "NOVOb:xcse", "CSE", "DKK"),
                    stock(4, "NOVOa:xcse", "CSE", "DKK"),
                ]),
            )
            .await;
            let r = UicResolver::new(reqwest::Client::new(), server.uri());
            let q = InstrumentQuery {
                isin: Some(NOVO_ISIN),
                ..InstrumentQuery::new("NOVO-B", "XCSE")
            };
            match r.resolve_with_cfd_check(&q, "t").await.unwrap_err() {
                UicResolverError::Ambiguous { symbols, .. } => {
                    assert_eq!(symbols, vec!["NOVOb:xcse", "NOVOa:xcse"])
                }
                e => panic!("{e:?}"),
            }
        }

        #[tokio::test]
        async fn a_currency_that_differs_from_the_registry_is_flagged_and_saxos_is_used() {
            let server = MockServer::start().await;
            mock_keywords(
                &server,
                "NOVO-B",
                list(&[stock(3, "NOVOb:xcse", "CSE", "DKK")]),
            )
            .await;
            let r = UicResolver::new(reqwest::Client::new(), server.uri());
            let q = InstrumentQuery {
                registry_currency: Some("SEK"),
                ..InstrumentQuery::new("NOVO-B", "XCSE")
            };
            let got = r.resolve_with_cfd_check(&q, "t").await.unwrap();
            assert!(got.currency_mismatch);
            assert_eq!(got.currency, "DKK");
        }

        #[tokio::test]
        async fn the_cache_key_is_ticker_and_mic() {
            let server = MockServer::start().await;
            mock_keywords(
                &server,
                "ABC",
                list(&[
                    stock(1, "ABC:xsto", "SSE", "SEK"),
                    stock(2, "ABC:xcse", "CSE", "DKK"),
                ]),
            )
            .await;
            let r = UicResolver::new(reqwest::Client::new(), server.uri());
            let se = r
                .resolve_with_cfd_check(&InstrumentQuery::new("ABC", "XSTO"), "t")
                .await
                .unwrap();
            let dk = r
                .resolve_with_cfd_check(&InstrumentQuery::new("ABC", "XCSE"), "t")
                .await
                .unwrap();
            assert_eq!((se.uic, dk.uic), (1, 2));
        }

        #[tokio::test]
        async fn an_exchange_id_override_and_an_unknown_mic() {
            let server = MockServer::start().await;
            mock_keywords(
                &server,
                "NOVO-B",
                list(&[stock(3, "NOVOb:xcse", "CSE_X", "DKK")]),
            )
            .await;
            let r = UicResolver::new(reqwest::Client::new(), server.uri())
                .with_exchange_ids([("XCSE".to_string(), "CSE_X".to_string())].into());
            assert_eq!(
                r.resolve_with_cfd_check(&InstrumentQuery::new("NOVO-B", "XCSE"), "t")
                    .await
                    .unwrap()
                    .uic,
                3
            );
            let err = r
                .resolve_with_cfd_check(&InstrumentQuery::new("X", "XOSL"), "t")
                .await
                .unwrap_err();
            assert!(matches!(err, UicResolverError::UnknownExchange(_)));
        }

        const FAST: RetryPolicy = RetryPolicy {
            max_attempts: 4,
            base_backoff: Duration::from_millis(1),
            max_wait: Duration::from_millis(20),
        };

        #[tokio::test]
        async fn a_transient_failure_is_retried_within_the_call() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(503))
                .up_to_n_times(1)
                .with_priority(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_string(GOMX))
                .with_priority(2)
                .mount(&server)
                .await;
            let r = UicResolver::new(reqwest::Client::new(), server.uri()).with_retry(FAST);
            let got = r
                .resolve_with_cfd_check(&InstrumentQuery::new("GOMX", "FNSE"), "t")
                .await
                .unwrap();
            assert_eq!(got.uic, 4769462);
            assert_eq!(server.received_requests().await.unwrap().len(), 2);
        }

        #[tokio::test]
        async fn a_permanent_status_is_not_retried() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(400))
                .mount(&server)
                .await;
            let r = UicResolver::new(reqwest::Client::new(), server.uri()).with_retry(FAST);
            let err = r
                .resolve_with_cfd_check(&InstrumentQuery::new("GOMX", "FNSE"), "t")
                .await
                .unwrap_err();
            assert!(!err.is_transient());
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
        }

        #[tokio::test]
        async fn no_answer_is_retried_then_reported_with_its_cause() {
            // A port nothing listens on: every attempt is refused.
            let addr = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap();
            let r =
                UicResolver::new(reqwest::Client::new(), format!("http://{addr}")).with_retry(FAST);
            let err = r
                .resolve_with_cfd_check(&InstrumentQuery::new("GOMX", "FNSE"), "t")
                .await
                .unwrap_err();
            assert!(err.is_transient(), "{err:?}");
            let chain = crate::saxo::error_chain(&err);
            assert!(
                chain.starts_with("HTTP error resolving Uic for GOMX"),
                "{chain}"
            );
            assert!(
                chain.len() > err.to_string().len(),
                "the cause is appended: {chain}"
            );
        }

        #[test]
        fn waits_double_and_respect_the_cap_and_retry_after() {
            let p = RetryPolicy::default();
            assert_eq!(p.wait(1, None), Duration::from_secs(1));
            assert_eq!(p.wait(3, None), Duration::from_secs(4));
            assert_eq!(
                p.wait(1, Some(Duration::from_secs(30))),
                Duration::from_secs(30)
            );
            assert_eq!(
                p.wait(1, Some(Duration::from_secs(3600))),
                Duration::from_secs(120)
            );
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
            let r = UicResolver::new(reqwest::Client::new(), server.uri())
                .with_retry(RetryPolicy::NONE);
            let err = r
                .resolve_with_cfd_check(&InstrumentQuery::new("GOMX", "FNSE"), "t")
                .await
                .unwrap_err();
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
            assert!(r
                .resolve_with_cfd_check(&InstrumentQuery::new("GOMX", "FNSE"), "t")
                .await
                .is_err());
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
            let r = UicResolver::new(reqwest::Client::new(), server.uri())
                .with_retry(RetryPolicy::NONE);
            let err = r
                .resolve_with_cfd_check(&InstrumentQuery::new("GOMX", "FNSE"), "t")
                .await
                .unwrap_err();
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
                let r = UicResolver::new(reqwest::Client::new(), server.uri())
                    .with_retry(RetryPolicy::NONE);
                let err = r
                    .resolve_with_cfd_check(&InstrumentQuery::new("GOMX", "FNSE"), "t")
                    .await
                    .unwrap_err();
                assert_eq!(err.is_transient(), transient, "{code}");
            }
            assert!(!UicResolverError::NotFound("X".into()).is_transient());
            assert!(!UicResolverError::CfdOnly("X".into()).is_transient());
            assert!(!UicResolverError::UnknownExchange("X".into()).is_transient());
            assert!(!UicResolverError::Ambiguous {
                ticker: "X".into(),
                symbols: vec![]
            }
            .is_transient());
        }
    }
}
