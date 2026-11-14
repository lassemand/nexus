//! The analyst's kernel runtime: loads a WASM kernel written against
//! `verdict-sdk`, runs it on a release text within a deadline and memory limit,
//! and returns the verdict.
//!
//! A kernel is AI-written code, so it runs in `wasmtime` with no imports (no
//! network, files or host memory), an epoch deadline and a memory cap. All
//! search lives inside the kernel's own `analyze`; the runtime knows nothing
//! about it and adds only a fixed, small per-call cost.
//!
//! Around the runtime sit the analyst's database ([`Store`]) and its settings
//! ([`Config`]) and the scheduler that decides when to start preparing a kernel
//! for each upcoming report ([`Scheduler`]) and the consumer of report notes
//! ([`NoteHandler`]). See the crate README.

pub mod calendar;
pub mod claims;
pub mod clock;
pub mod config;
pub mod decision;
mod error;
pub mod handoff;
pub mod matching;
pub mod metrics;
pub mod notes;
pub mod offsets;
pub mod pnl;
pub mod quotes;
pub mod registry;
pub mod release;
pub mod releases;
pub mod runner;
mod runtime;
pub mod schedule;
pub mod scheduler;
pub mod sink;
pub mod spans;
pub mod store;
pub mod trades;
pub mod trading;
mod types;
mod verdict;

pub use calendar::SessionsCache;
pub use clock::{Clock, ManualClock, SystemClock, TokioClock};
pub use config::{parse_horizons, Config, ConfigError, Horizon};
pub use decision::{DecisionKind, NewDecision, Recorded, StoredDecision};
pub use error::{KernelError, LoadError};
pub use handoff::{
    decision_channel, ChannelHandOff, DecisionHandle, DecisionSlot, NoopHandOff, Persisted,
    TradeHandOff, TradeSignal,
};
pub use matching::{pick_target, MatchKind, Matched, Release};
pub use notes::{NoteHandler, NoteOutcome};
pub use pnl::{compute as compute_pnl, Pnl, PnlProblem};
pub use quotes::{HttpQuoteSource, Quote, QuoteError, QuoteSource};
pub use registry::{preload, KernelRegistry, PreloadReport, KERNEL_RETENTION};
pub use release::{
    classify, is_transient, with_retry, with_retry_counted, Classified, Processed, Receipt,
    ReleaseInput, ReleaseProcessor, RetryPolicy,
};
pub use releases::{consumer_span, prepare, run_release_consumer, Prepared, RELEASES_GROUP};
pub use runner::{RunnerSettings, TradeRunner, EXIT_RETRY_FOR, LATE_AFTER, RETRY_EVERY};
pub use runtime::{
    run_blocking, KernelId, KernelRuntime, LoadTimings, LoadedKernel, RunOutcome, RuntimeConfig,
    KERNEL_DEADLINE, MAX_INPUT_BYTES, MAX_MODULE_BYTES, MAX_OUTPUT_BYTES, MEMORY_LIMIT_BYTES,
    WARM_SAMPLE,
};
pub use schedule::SignalSchedule;
pub use scheduler::{
    LaunchError, LaunchInfo, NoopLauncher, PassReport, PrepLauncher, PrepRequest, RunResult,
    Scheduler, SchedulerConfig, SchedulerError,
};
pub use sink::{trace_headers, DecisionSink, KafkaDecisionSink, KafkaTradeSink, TradeSink};
pub use spans::trace_id_of;
pub use store::{AcceptedKernel, NewNote, Note, PrepRun, Store, Target};
pub use trades::{
    to_proto as trade_to_proto, Entry, Exit, NewObservation, NewTrade, ObservationStatus,
    StoredObservation, StoredTrade, TradeStatus, TradeStore,
};
pub use trading::{exit_target, observation_slots, plan_entry, session_cutoff, EntryPlan, Slot};
pub use types::{ReportKind, ScheduledReport, TargetStatus};
pub use verdict::{KernelMetric, KernelVerdict};
