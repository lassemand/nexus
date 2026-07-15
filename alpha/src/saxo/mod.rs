/// Saxo Bank OpenAPI WebSocket client for real-time Nordic market data.
///
/// # Architecture
///
/// This module is **push-based** and intentionally does NOT implement the
/// [`crate::bar_provider::BarProvider`] trait. `BarProvider` is pull/request-response
/// shaped (one call → one result). The Saxo stream delivers ticks continuously; a
/// caller owns a [`SaxoBarStream`] and polls it for completed OHLCV bars. This
/// design separation is deliberate — do not add a `BarProvider` impl here.
///
/// # Data flow
///
/// ```text
/// Saxo OAuth2 token ──────────────────────────────────────────────────────┐
///                                                                         │ refresh via
/// Registered FNSE tickers → Uic resolver → (Uic, Asset) pairs            │ PUT /ws/authorize
///       │                                       │                         │ (no reconnect)
///       │                           WebSocket subscription                │
///       │                                       │                         │
///       └──────────────── tick stream ──────────►  Bar aggregator         │
///                          (1-min OHLCV)         │                        │
///                                                ▼                        │
///                                           mpsc channel ◄────────────────┘
///                                                │
///                                          caller receives Bar
/// ```
///
/// # Token refresh
///
/// Per ADR-0001 / NEX-77: Saxo supports renewing the access token on an
/// existing WebSocket without reconnecting, via:
/// ```text
/// PUT https://sim-streaming.saxobank.com/sim/oapi/streaming/ws/authorize
///   ?contextid=<ContextId>
/// Authorization: Bearer <new_access_token>
/// ```
/// This module uses that mechanism — the WebSocket connection and all subscriptions
/// survive token refresh.
///
/// # APA/OTC coverage gap
///
/// As documented in ADR-0001 (NEX-77), it is unconfirmed whether Saxo's
/// `Stock` market data feed for FNSE instruments includes APA-reported
/// off-exchange trades or only primary lit-book prints. Until confirmed,
/// treat bars produced here as potentially incomplete for instruments with
/// significant OTC/dark pool activity. See NEX-87 for verification plan.
///
/// # Reconnect strategy
///
/// On WebSocket disconnect:
/// 1. Exponential backoff starting at 1s, capped at 60s.
/// 2. On reconnect, attempt gap-fill via `GET /chart/v1/charts` for the
///    period since the last received tick.
/// 3. During normal non-trading hours (per [`model::calendar::StockholmCalendar`]),
///    the heartbeat timeout is suppressed — no reconnect loop during weekend/holiday.
pub mod aggregator;
pub mod auth;
pub mod stream;
pub mod uic;
pub mod validation;

pub use aggregator::BarAggregator;
pub use auth::{SaxoAuth, SaxoToken};
pub use stream::{SaxoBarStream, SaxoConfig};
pub use uic::{ResolvedUic, UicResolver, UicResolverError};
pub use validation::{
    GapBoundary, GapClassifier, OhlcError, OhlcValidator, SilenceCause, TickDeduplicator,
};
