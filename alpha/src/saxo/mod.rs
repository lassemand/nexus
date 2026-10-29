/// Saxo Bank OpenAPI: OAuth tokens, instrument (Uic) resolution and official
/// daily bars from the chart API.
///
/// The live WebSocket price stream that used to live here was removed
/// (NEX-193): nothing consumed its 1-minute bars. Daily bars for Nordic report
/// reactions come from [`chart::ChartClient`].
pub mod auth;
pub mod chart;
pub mod uic;

/// Whether a Saxo response status can succeed on a later try: request
/// timeout, rate limit or a server error. Every other 4xx (bad request,
/// unauthorized, not found, …) gives the same answer again, so it is not
/// retried.
pub fn is_transient_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

/// Whether a failed request can succeed on a later try: no response at all
/// (connect, TLS, timeout), or a transient status.
pub fn is_transient_reqwest(e: &reqwest::Error) -> bool {
    e.status().is_none_or(is_transient_status)
}

/// `err` followed by each of its sources, `": "`-separated. A source whose
/// text the line already contains is skipped (reqwest repeats its own).
///
/// reqwest's `Display` is only the top line ("error sending request for
/// url ..."); the cause (DNS, connect, TLS) is in the source chain.
pub fn error_chain(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(e) = source {
        let text = e.to_string();
        if !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        source = e.source();
    }
    out
}

pub use auth::{RotatedToken, SaxoAuth, SaxoToken, SharedToken, TokenStore};
pub use chart::{ChartClient, ChartError, DailyBarRow};
pub use uic::{
    exchange_ids, InstrumentQuery, ResolvedUic, RetryPolicy, UicResolver, UicResolverError,
};
