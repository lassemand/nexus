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
//! ([`Config`]). See the crate README.

pub mod clock;
pub mod config;
mod error;
pub mod metrics;
mod runtime;
pub mod store;
mod types;
mod verdict;

pub use clock::{Clock, ManualClock, SystemClock};
pub use config::{Config, ConfigError};
pub use error::{KernelError, LoadError};
pub use runtime::{
    run_blocking, KernelId, KernelRuntime, LoadTimings, LoadedKernel, RunOutcome, RuntimeConfig,
    KERNEL_DEADLINE, MAX_INPUT_BYTES, MAX_MODULE_BYTES, MAX_OUTPUT_BYTES, MEMORY_LIMIT_BYTES,
    WARM_SAMPLE,
};
pub use store::{AcceptedKernel, PrepRun, Store, Target};
pub use types::{ReportKind, ScheduledReport, TargetStatus};
pub use verdict::{KernelMetric, KernelVerdict};
