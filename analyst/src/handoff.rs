use chrono::{DateTime, Utc};
use telemetry::trace::opentelemetry::Context;
use tokio::sync::{mpsc, watch};

use crate::decision::DecisionKind;

/// The decision row as it was stored, told to the trade after the write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Persisted {
    /// The `decisions` row id.
    pub id: i64,
    /// The decision as stored: a version that lost a race is `twin_ignored`, and
    /// then there is no trade.
    pub kind: DecisionKind,
}

/// Tells the trade which row its decision became. Dropped without a value when
/// the write failed or the release turned out to be a duplicate.
#[derive(Debug)]
pub struct DecisionSlot(watch::Sender<Option<Persisted>>);

impl DecisionSlot {
    /// Tells the trade the decision is stored.
    pub fn set(&self, persisted: Persisted) {
        self.0.send_replace(Some(persisted));
    }
}

/// What the trade waits on to learn its decision's row, so that the first quote
/// request never waits for the database.
#[derive(Debug, Clone)]
pub struct DecisionHandle(watch::Receiver<Option<Persisted>>);

impl DecisionHandle {
    /// A handle that already holds `persisted`.
    pub fn ready(persisted: Persisted) -> DecisionHandle {
        let (_tx, rx) = watch::channel(Some(persisted));
        DecisionHandle(rx)
    }

    /// Waits until the decision is stored. `None` when it never will be.
    pub async fn wait(&mut self) -> Option<Persisted> {
        loop {
            if let Some(persisted) = *self.0.borrow_and_update() {
                return Some(persisted);
            }
            if self.0.changed().await.is_err() {
                return *self.0.borrow();
            }
        }
    }
}

/// A slot for the trade of one decision, and the handle the trade waits on.
pub fn decision_channel() -> (DecisionSlot, DecisionHandle) {
    let (tx, rx) = watch::channel(None);
    (DecisionSlot(tx), DecisionHandle(rx))
}

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
    /// Resolves to the stored decision row once the database write is done.
    pub persisted: DecisionHandle,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_handle_waits_for_the_slot_and_ends_when_it_is_dropped() {
        let (slot, mut handle) = decision_channel();
        let waiter = tokio::spawn(async move { handle.wait().await });
        tokio::task::yield_now().await;
        let stored = Persisted {
            id: 4,
            kind: DecisionKind::Buy,
        };
        slot.set(stored);
        assert_eq!(waiter.await.expect("task"), Some(stored));

        let (slot, mut handle) = decision_channel();
        drop(slot);
        assert_eq!(handle.wait().await, None);
        assert_eq!(DecisionHandle::ready(stored).wait().await, Some(stored));
    }
}
