use chrono::{DateTime, Utc};
use telemetry::trace::opentelemetry::Context;
use tokio::sync::mpsc;

use crate::decision::DecisionKind;

/// A decision handed to the trade scheduler, which simulates the trade.
#[derive(Debug, Clone)]
pub struct TradeSignal {
    /// MFN's news id of the release.
    pub news_id: String,
    /// The company's ticker.
    pub ticker: String,
    /// The company's exchange.
    pub exchange_mic: String,
    /// The release's language.
    pub lang: String,
    /// The release's fiscal period.
    pub fiscal_period: Option<String>,
    /// What was decided.
    pub decision: DecisionKind,
    /// True for `skipped_no_kernel`: simulated as a baseline, marked as such.
    pub baseline: bool,
    /// When MFN published the release.
    pub published_at: DateTime<Utc>,
    /// When the analyst consumed it.
    pub received_at: DateTime<Utc>,
    /// When it was decided.
    pub decided_at: DateTime<Utc>,
    /// The OpenTelemetry context of the `analyst.hand_off` span, so that the
    /// simulated trade's spans join the release's trace.
    pub context: Context,
}

/// Where a decision goes the moment it is made, before the database write, so
/// that starting the trade never waits for the database.
///
/// `hand_off` is synchronous and must not block: it is on the release path.
pub trait TradeHandOff: Send + Sync {
    /// Hands over a decision.
    fn hand_off(&self, signal: TradeSignal);
}

/// Drops every decision. The service uses it until the trade scheduler exists.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopHandOff;

impl TradeHandOff for NoopHandOff {
    fn hand_off(&self, _signal: TradeSignal) {}
}

/// Sends every decision on an unbounded in-memory channel, which never blocks.
#[derive(Debug, Clone)]
pub struct ChannelHandOff(mpsc::UnboundedSender<TradeSignal>);

impl ChannelHandOff {
    /// A hand-off and the receiver the trade scheduler reads.
    pub fn channel() -> (ChannelHandOff, mpsc::UnboundedReceiver<TradeSignal>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (ChannelHandOff(tx), rx)
    }
}

impl TradeHandOff for ChannelHandOff {
    fn hand_off(&self, signal: TradeSignal) {
        let _ = self.0.send(signal);
    }
}
