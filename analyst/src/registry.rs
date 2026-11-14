use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::{info, warn};

use crate::runtime::{KernelRuntime, LoadedKernel};
use crate::store::Store;

/// How long a decided report's kernel instance stays in memory: twins of the
/// report may still arrive.
pub const KERNEL_RETENTION: Duration = Duration::from_secs(60 * 60);

/// The loaded, warmed kernels the release path runs, by report target.
///
/// A kernel is compiled and warmed when it is accepted (or at startup), long
/// before its release, so that nothing but a call is left to do when the report
/// comes out.
#[derive(Default)]
pub struct KernelRegistry {
    kernels: Mutex<HashMap<i64, Arc<LoadedKernel>>>,
}

impl KernelRegistry {
    /// An empty registry.
    pub fn new() -> KernelRegistry {
        KernelRegistry::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<i64, Arc<LoadedKernel>>> {
        self.kernels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The kernel loaded for a target.
    pub fn get(&self, target_id: i64) -> Option<Arc<LoadedKernel>> {
        self.lock().get(&target_id).cloned()
    }

    /// Loads `kernel` for a target, replacing and returning an earlier one.
    pub fn insert(&self, target_id: i64, kernel: Arc<LoadedKernel>) -> Option<Arc<LoadedKernel>> {
        self.lock().insert(target_id, kernel)
    }

    /// Drops a target's kernel.
    pub fn remove(&self, target_id: i64) -> Option<Arc<LoadedKernel>> {
        self.lock().remove(&target_id)
    }

    /// How many kernels are loaded.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// True when no kernel is loaded.
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Drops a target's kernel after `after`, unless another kernel has replaced
    /// it by then.
    pub fn schedule_drop(
        self: &Arc<Self>,
        target_id: i64,
        kernel: &Arc<LoadedKernel>,
        after: Duration,
    ) {
        let registry = Arc::clone(self);
        let kernel = Arc::clone(kernel);
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            let mut kernels = registry.lock();
            if kernels
                .get(&target_id)
                .is_some_and(|current| Arc::ptr_eq(current, &kernel))
            {
                kernels.remove(&target_id);
            }
        });
    }
}

/// What [`preload`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreloadReport {
    /// Kernels loaded and warmed.
    pub loaded: usize,
    /// Kernels that could not be loaded; the report will be recorded as having no kernel.
    pub failed: usize,
}

/// Loads every accepted kernel of a report that is not yet decided, so that a
/// restart loses none. Compiling is CPU work, so each kernel loads on the
/// blocking pool.
pub async fn preload(
    store: &Store,
    runtime: &Arc<KernelRuntime>,
    registry: &Arc<KernelRegistry>,
) -> Result<PreloadReport, sqlx::Error> {
    let mut report = PreloadReport {
        loaded: 0,
        failed: 0,
    };
    for stored in store.accepted_kernels_to_load().await? {
        let runtime = Arc::clone(runtime);
        let (kernel_id, target_id) = (stored.kernel_id, stored.target_id);
        let loaded = tokio::task::spawn_blocking(move || {
            runtime.load(u64::try_from(stored.kernel_id).unwrap_or(0), &stored.wasm)
        })
        .await;
        match loaded {
            Ok(Ok(kernel)) => {
                registry.insert(target_id, Arc::new(kernel));
                report.loaded += 1;
            }
            Ok(Err(error)) => {
                warn!(kernel_id, target_id, %error, "an accepted kernel could not be loaded");
                report.failed += 1;
            }
            Err(error) => {
                warn!(kernel_id, target_id, %error, "loading a kernel panicked");
                report.failed += 1;
            }
        }
    }
    info!(
        loaded = report.loaded,
        failed = report.failed,
        "accepted kernels preloaded"
    );
    Ok(report)
}

/// An accepted kernel's bytes, as stored.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct StoredKernel {
    /// The kernel's id.
    pub kernel_id: i64,
    /// The report target it belongs to.
    pub target_id: i64,
    /// The WASM module.
    pub wasm: Vec<u8>,
}

impl Store {
    /// The accepted kernels of reports that can still be released: not decided,
    /// skipped or cancelled.
    pub async fn accepted_kernels_to_load(&self) -> Result<Vec<StoredKernel>, sqlx::Error> {
        sqlx::query_as(
            "SELECT k.id AS kernel_id, k.target_id, k.wasm
             FROM kernels k JOIN report_targets t ON t.id = k.target_id
             WHERE k.status = 'accepted' AND t.status IN ('waiting', 'preparing', 'ready', 'failed')
             ORDER BY k.id",
        )
        .fetch_all(&self.pool)
        .await
    }
}
