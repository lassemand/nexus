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

pub use auth::{RotatedToken, SaxoAuth, SaxoToken, SharedToken, TokenStore};
pub use chart::{ChartClient, ChartError, DailyBarRow};
pub use uic::{exchange_ids, InstrumentQuery, ResolvedUic, UicResolver, UicResolverError};
