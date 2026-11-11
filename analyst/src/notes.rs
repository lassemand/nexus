use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use model::generated::ReportNote;
use prost::Message;
use rdkafka::consumer::{CommitMode, Consumer, StreamConsumer};
use rdkafka::{ClientConfig, Message as KafkaMessage};
use thiserror::Error;
use tokio::sync::Notify;
use tracing::{info, warn};
use uuid::Uuid;

use crate::clock::Clock;
use crate::config::MAX_NOTE_BYTES;
use crate::metrics;
use crate::schedule::SignalSchedule;
use crate::store::{NewNote, Store};

/// The Kafka consumer group of the analyst.
pub const CONSUMER_GROUP: &str = "analyst";

const RETRY_START: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);

/// What became of one note event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteOutcome {
    /// The note was stored.
    Stored,
    /// The note was already stored.
    Duplicate,
    /// The company is not registered.
    RejectedUnregistered,
    /// The note is over 4 KiB.
    RejectedTooLarge,
    /// The event is not a valid note; the reason says why.
    RejectedInvalid(String),
}

impl NoteOutcome {
    /// The label of `analyst_notes_total`.
    pub fn label(&self) -> &'static str {
        match self {
            NoteOutcome::Stored => "stored",
            NoteOutcome::Duplicate => "duplicate",
            NoteOutcome::RejectedUnregistered => "rejected_unregistered",
            NoteOutcome::RejectedTooLarge => "rejected_too_large",
            NoteOutcome::RejectedInvalid(_) => "rejected_invalid",
        }
    }
}

/// A database was unavailable; the note is neither stored nor rejected, so the
/// event must be handled again.
#[derive(Debug, Error)]
#[error("database error: {0}")]
pub struct NoteError(#[from] pub sqlx::Error);

/// Stores note events and wakes the scheduler.
pub struct NoteHandler {
    store: Store,
    signal: SignalSchedule,
    clock: Arc<dyn Clock>,
    wake: Arc<Notify>,
}

impl NoteHandler {
    /// A handler that stores into `store`, checks companies against `signal`,
    /// stamps notes with `clock` and notifies `wake` after storing one.
    pub fn new(
        store: Store,
        signal: SignalSchedule,
        clock: Arc<dyn Clock>,
        wake: Arc<Notify>,
    ) -> NoteHandler {
        NoteHandler {
            store,
            signal,
            clock,
            wake,
        }
    }

    /// Handles one event: decodes and validates it, rejects what it must,
    /// stores the rest once by its `note_id`, and notifies the scheduler.
    /// Rejections are logged and counted, not errors; only an unavailable
    /// database is.
    pub async fn handle(&self, payload: &[u8]) -> Result<NoteOutcome, NoteError> {
        let outcome = self.decide(payload).await?;
        metrics::note(outcome.label());
        match &outcome {
            NoteOutcome::Stored => self.wake.notify_one(),
            NoteOutcome::Duplicate => info!("note already stored"),
            NoteOutcome::RejectedUnregistered => {
                warn!("note rejected: the company is not registered")
            }
            NoteOutcome::RejectedTooLarge => warn!("note rejected: over {MAX_NOTE_BYTES} bytes"),
            NoteOutcome::RejectedInvalid(reason) => warn!(%reason, "note rejected: not valid"),
        }
        Ok(outcome)
    }

    async fn decide(&self, payload: &[u8]) -> Result<NoteOutcome, NoteError> {
        let note = match ReportNote::decode(payload) {
            Ok(note) => note,
            Err(error) => {
                return Ok(NoteOutcome::RejectedInvalid(format!(
                    "undecodable: {error}"
                )))
            }
        };
        if note.text.len() > MAX_NOTE_BYTES {
            return Ok(NoteOutcome::RejectedTooLarge);
        }
        let Ok(source_event_id) = Uuid::parse_str(note.note_id.trim()) else {
            return Ok(NoteOutcome::RejectedInvalid(
                "note_id is not a UUID".to_owned(),
            ));
        };
        let (ticker, exchange_mic) = (note.ticker.trim(), note.exchange_mic.trim());
        if ticker.is_empty() || exchange_mic.is_empty() {
            return Ok(NoteOutcome::RejectedInvalid(
                "ticker and exchange_mic are required".to_owned(),
            ));
        }
        if note.text.trim().is_empty() {
            return Ok(NoteOutcome::RejectedInvalid("the note is empty".to_owned()));
        }
        if !self.signal.is_registered(ticker, exchange_mic).await? {
            return Ok(NoteOutcome::RejectedUnregistered);
        }
        let now = self.clock.now();
        let period = note.fiscal_period.trim();
        let stored = self
            .store
            .insert_note(&NewNote {
                ticker: ticker.to_owned(),
                exchange_mic: exchange_mic.to_owned(),
                fiscal_period: (!period.is_empty()).then(|| period.to_owned()),
                text: note.text,
                author: note.author.trim().to_owned(),
                created_at: authored_at(note.created_at_unix_secs, now),
                received_at: now,
                source_event_id,
            })
            .await?;
        Ok(if stored {
            NoteOutcome::Stored
        } else {
            NoteOutcome::Duplicate
        })
    }
}

fn authored_at(unix_secs: i64, now: DateTime<Utc>) -> DateTime<Utc> {
    match Utc.timestamp_opt(unix_secs, 0).single() {
        Some(at) if unix_secs > 0 && at <= now + chrono::Duration::days(1) => at,
        _ => now,
    }
}

/// Consumes `topic` in the `analyst` group until `shutdown` completes. An
/// event is committed only after it was handled; while a database is
/// unavailable the same event is retried with a growing pause, which holds the
/// partition rather than lose a note.
pub async fn run_consumer(
    brokers: &str,
    topic: &str,
    handler: &NoteHandler,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<(), rdkafka::error::KafkaError> {
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", brokers)
        .set("group.id", CONSUMER_GROUP)
        .set("auto.offset.reset", "earliest")
        .set("enable.auto.commit", "false")
        .create()?;
    consumer.subscribe(&[topic])?;
    info!(%brokers, %topic, group = CONSUMER_GROUP, "notes consumer started");
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        let message = tokio::select! {
            () = &mut shutdown => return Ok(()),
            received = consumer.recv() => received,
        };
        let message = match message {
            Ok(message) => message,
            Err(error) => {
                warn!(%error, "notes consumer error");
                tokio::time::sleep(RETRY_START).await;
                continue;
            }
        };
        let mut pause = RETRY_START;
        loop {
            let handled = match message.payload() {
                Some(payload) => handler.handle(payload).await.map(|_| ()),
                None => Ok(()),
            };
            match handled {
                Ok(()) => break,
                Err(error) => {
                    warn!(%error, retry_in = ?pause, "note not handled, will retry");
                    tokio::time::sleep(pause).await;
                    pause = (pause * 2).min(RETRY_MAX);
                }
            }
        }
        if let Err(error) = consumer.commit_message(&message, CommitMode::Async) {
            warn!(%error, "could not commit the notes offset");
        }
    }
}
