use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use verdict_sdk::Lang;
use wasmtime::{
    Config, Engine, ExternType, FuncType, InstancePre, Linker, Memory, Module, OptLevel, Store,
    StoreLimits, StoreLimitsBuilder, Trap, TypedFunc, ValType,
};

use crate::verdict::KernelVerdict;
use crate::{metrics, KernelError, LoadError};

/// The default time a kernel may run before it is interrupted.
pub const KERNEL_DEADLINE: Duration = Duration::from_millis(50);
/// The largest module accepted.
pub const MAX_MODULE_BYTES: usize = 8 * 1024 * 1024;
/// The largest release text passed to a kernel.
pub const MAX_INPUT_BYTES: usize = 1024 * 1024;
/// The largest output a kernel may return.
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// The most memory a kernel instance may use.
pub const MEMORY_LIMIT_BYTES: usize = 64 * 1024 * 1024;
/// A built-in release used to warm a kernel up; it belongs to no company.
pub const WARM_SAMPLE: &str = "Sample Company AB: Interim report\n\n\
    Net sales increased to SEK 1,234.5 million (1,100.0). \
    Operating profit was SEK 56.7 million (-12.3).\n";

const TABLE_ELEMENTS: usize = 10_000;
const EPOCH_TICK: Duration = Duration::from_millis(1);
const MAX_TICK_LAG: Duration = Duration::from_millis(50);

/// How long each step of loading a kernel took.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LoadTimings {
    /// Size, validity, imports and exports.
    pub validate: Duration,
    /// Cranelift compilation.
    pub compile: Duration,
    /// Creating the first instance.
    pub instantiate: Duration,
    /// The two warm-up calls (empty text, the built-in sample); the first one
    /// builds the kernel's statics.
    pub warm: Duration,
}

/// Identifies a kernel in logs and errors; the database id comes later.
pub type KernelId = u64;

/// Limits of a [`KernelRuntime`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeConfig {
    /// How long one kernel call may run. Rounded to whole milliseconds, at least one.
    pub deadline: Duration,
    /// The largest release text passed to a kernel.
    pub max_input_bytes: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        RuntimeConfig {
            deadline: KERNEL_DEADLINE,
            max_input_bytes: MAX_INPUT_BYTES,
        }
    }
}

impl RuntimeConfig {
    /// The default limits, with the deadline from `KERNEL_DEADLINE_MS` when set
    /// to a whole number of milliseconds.
    pub fn from_env() -> RuntimeConfig {
        let mut config = RuntimeConfig::default();
        if let Some(ms) = std::env::var("KERNEL_DEADLINE_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
        {
            config.deadline = Duration::from_millis(ms);
        }
        config
    }

    fn deadline_ticks(&self) -> u64 {
        let ticks = self.deadline.as_millis() / EPOCH_TICK.as_millis();
        u64::try_from(ticks).unwrap_or(u64::MAX).max(1)
    }
}

struct Ticker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Ticker {
    fn start(engine: Engine) -> std::io::Result<Ticker> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("analyst-epoch".to_owned())
            .spawn(move || {
                let mut next = Instant::now() + EPOCH_TICK;
                while !flag.load(Ordering::Relaxed) {
                    let now = Instant::now();
                    if now < next {
                        thread::sleep(next - now);
                        continue;
                    }
                    if now - next > MAX_TICK_LAG {
                        next = now;
                    }
                    engine.increment_epoch();
                    next += EPOCH_TICK;
                }
            })?;
        Ok(Ticker {
            stop,
            handle: Some(handle),
        })
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct StoreData {
    limits: StoreLimits,
}

struct Live {
    store: Store<StoreData>,
    memory: Memory,
    input: TypedFunc<i32, i32>,
    run: TypedFunc<(i32, i32), i64>,
}

/// One shared `wasmtime` engine and the epoch thread that enforces deadlines.
/// Create one and [`load`](KernelRuntime::load) every kernel through it.
pub struct KernelRuntime {
    engine: Engine,
    config: RuntimeConfig,
    ticker: Arc<Ticker>,
}

impl KernelRuntime {
    /// Creates the engine (Cranelift speed optimisation, epoch interruption,
    /// SIMD, no WASI, no fuel) and starts the thread that advances the epoch
    /// every millisecond of wall time. Where a sleep lasts longer than asked
    /// (a coarse timer), the thread catches up on the ticks it missed, so a
    /// deadline is late by at most one sleep, never by a multiple.
    pub fn new(config: RuntimeConfig) -> anyhow::Result<KernelRuntime> {
        let mut engine_config = Config::new();
        engine_config.cranelift_opt_level(OptLevel::Speed);
        engine_config.epoch_interruption(true);
        engine_config.wasm_simd(true);
        let engine = Engine::new(&engine_config).map_err(|error| anyhow::anyhow!("{error:#}"))?;
        let ticker = Arc::new(Ticker::start(engine.clone())?);
        Ok(KernelRuntime {
            engine,
            config,
            ticker,
        })
    }

    /// Validates, compiles, instantiates and warms a kernel.
    ///
    /// Compiles from `wasm` every time; nothing precompiled is ever persisted,
    /// because a precompiled module only loads in the `wasmtime` version that
    /// built it.
    pub fn load(&self, id: KernelId, wasm: &[u8]) -> Result<LoadedKernel, LoadError> {
        let result = self.load_inner(id, wasm);
        if let Err(error) = &result {
            tracing::warn!(kernel_id = id, sha256 = %sha256_hex(wasm), %error, "kernel load failed");
        }
        result
    }

    fn load_inner(&self, id: KernelId, wasm: &[u8]) -> Result<LoadedKernel, LoadError> {
        let started = Instant::now();
        if wasm.len() > MAX_MODULE_BYTES {
            return Err(LoadError::TooLarge {
                size: wasm.len(),
                max: MAX_MODULE_BYTES,
            });
        }
        Module::validate(&self.engine, wasm)
            .map_err(|error| LoadError::Invalid(format!("{error:#}")))?;
        let mut validate = started.elapsed();

        let compile_started = Instant::now();
        let module = Module::new(&self.engine, wasm)
            .map_err(|error| LoadError::Compile(format!("{error:#}")))?;
        let compile_elapsed = compile_started.elapsed();
        metrics::load_step("compile", compile_elapsed);

        let checks_started = Instant::now();
        check_module(&module)?;
        validate += checks_started.elapsed();
        metrics::load_step("validate", validate);

        let pre = Linker::<StoreData>::new(&self.engine)
            .instantiate_pre(&module)
            .map_err(|error| LoadError::Instantiate(format!("{error:#}")))?;
        let mut kernel = LoadedKernel {
            timings: LoadTimings::default(),
            id,
            sha256: sha256_hex(wasm),
            size: wasm.len(),
            engine: self.engine.clone(),
            config: self.config,
            pre,
            live: Mutex::new(None),
            _ticker: Arc::clone(&self.ticker),
        };

        let instantiate_started = Instant::now();
        let mut live = kernel.instantiate().map_err(LoadError::Instantiate)?;
        let instantiate = instantiate_started.elapsed();
        metrics::load_step("instantiate", instantiate);

        let warm_started = Instant::now();
        kernel.warm(&mut live).map_err(LoadError::Warm)?;
        let warm = warm_started.elapsed();
        metrics::load_step("warm", warm);
        kernel.timings = LoadTimings {
            validate,
            compile: compile_elapsed,
            instantiate,
            warm,
        };

        *kernel.lock() = Some(live);
        metrics::loaded(1.0);
        Ok(kernel)
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn is_func(
    ty: Option<ExternType>,
    params: &[fn(&ValType) -> bool],
    results: &[fn(&ValType) -> bool],
) -> bool {
    let Some(ExternType::Func(func)) = ty else {
        return false;
    };
    func_matches(&func, params, results)
}

fn func_matches(
    func: &FuncType,
    params: &[fn(&ValType) -> bool],
    results: &[fn(&ValType) -> bool],
) -> bool {
    func.params().len() == params.len()
        && func.results().len() == results.len()
        && func.params().zip(params).all(|(ty, check)| check(&ty))
        && func.results().zip(results).all(|(ty, check)| check(&ty))
}

fn is_i32(ty: &ValType) -> bool {
    matches!(ty, ValType::I32)
}

fn is_i64(ty: &ValType) -> bool {
    matches!(ty, ValType::I64)
}

fn check_module(module: &Module) -> Result<(), LoadError> {
    if let Some(import) = module.imports().next() {
        return Err(LoadError::Imports(format!(
            "{}.{}",
            import.module(),
            import.name()
        )));
    }
    if !matches!(module.get_export("memory"), Some(ExternType::Memory(_))) {
        return Err(LoadError::Exports(
            "`memory` is missing or not a memory".to_owned(),
        ));
    }
    if !is_func(module.get_export("verdict_input"), &[is_i32], &[is_i32]) {
        return Err(LoadError::Exports(
            "`verdict_input` is missing or is not (i32) -> i32".to_owned(),
        ));
    }
    if !is_func(
        module.get_export("verdict_run"),
        &[is_i32, is_i32],
        &[is_i64],
    ) {
        return Err(LoadError::Exports(
            "`verdict_run` is missing or is not (i32, i32) -> i64".to_owned(),
        ));
    }
    Ok(())
}

/// What a run produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// The kernel returned a valid verdict.
    Ok {
        /// The parsed verdict.
        verdict: KernelVerdict,
        /// Wall time of the whole call, in microseconds: copying the text in,
        /// running the kernel, reading and parsing the output.
        micros: u64,
    },
    /// The run failed; the instance was discarded and rebuilt.
    Err(KernelError),
}

impl RunOutcome {
    /// The verdict and the microseconds, or the error.
    pub fn into_result(self) -> Result<(KernelVerdict, u64), KernelError> {
        match self {
            RunOutcome::Ok { verdict, micros } => Ok((verdict, micros)),
            RunOutcome::Err(error) => Err(error),
        }
    }
}

/// A compiled, instantiated and warmed kernel. `run` takes `&self`: the
/// instance sits behind its own mutex, so different kernels run in parallel
/// while one kernel's calls are serial.
pub struct LoadedKernel {
    timings: LoadTimings,
    id: KernelId,
    sha256: String,
    size: usize,
    engine: Engine,
    config: RuntimeConfig,
    pre: InstancePre<StoreData>,
    live: Mutex<Option<Live>>,
    _ticker: Arc<Ticker>,
}

impl Drop for LoadedKernel {
    fn drop(&mut self) {
        metrics::loaded(-1.0);
    }
}

impl LoadedKernel {
    /// The id the kernel was loaded under.
    pub fn id(&self) -> KernelId {
        self.id
    }

    /// The SHA-256 of the module's bytes, in lowercase hex.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// The module's size in bytes.
    pub fn size(&self) -> usize {
        self.size
    }

    /// How long each step of loading took.
    pub fn timings(&self) -> LoadTimings {
        self.timings
    }

    fn lock(&self) -> MutexGuard<'_, Option<Live>> {
        self.live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn instantiate(&self) -> Result<Live, String> {
        let mut store = Store::new(
            &self.engine,
            StoreData {
                limits: StoreLimitsBuilder::new()
                    .memory_size(MEMORY_LIMIT_BYTES)
                    .instances(1)
                    .memories(1)
                    .tables(1)
                    .table_elements(TABLE_ELEMENTS)
                    .trap_on_grow_failure(true)
                    .build(),
            },
        );
        store.limiter(|data| &mut data.limits);
        store.set_epoch_deadline(self.config.deadline_ticks());
        let instance = self
            .pre
            .instantiate(&mut store)
            .map_err(|error| format!("{error:#}"))?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| "`memory` is not a memory".to_owned())?;
        let input = instance
            .get_typed_func::<i32, i32>(&mut store, "verdict_input")
            .map_err(|error| format!("{error:#}"))?;
        let run = instance
            .get_typed_func::<(i32, i32), i64>(&mut store, "verdict_run")
            .map_err(|error| format!("{error:#}"))?;
        Ok(Live {
            store,
            memory,
            input,
            run,
        })
    }

    fn warm(&self, live: &mut Live) -> Result<(), KernelError> {
        self.call(live, b"", Lang::Other, |_| Ok(()))?;
        self.call(live, WARM_SAMPLE.as_bytes(), Lang::En, |_| Ok(()))
    }

    fn fresh(&self) -> Result<Live, KernelError> {
        let mut live = self.instantiate().map_err(KernelError::Trap)?;
        self.warm(&mut live)?;
        Ok(live)
    }

    fn call<R>(
        &self,
        live: &mut Live,
        text: &[u8],
        lang: Lang,
        read: impl FnOnce(&[u8]) -> Result<R, KernelError>,
    ) -> Result<R, KernelError> {
        live.store.set_epoch_deadline(self.config.deadline_ticks());
        let len = i32::try_from(text.len()).map_err(|_| KernelError::InputTooLarge)?;
        let pointer = live.input.call(&mut live.store, len).map_err(classify)?;
        live.memory
            .write(&mut live.store, pointer as u32 as usize, text)
            .map_err(|_| KernelError::BadOutput("input buffer lies outside memory".to_owned()))?;
        let lang_code = i32::try_from(lang.code()).unwrap_or(0);
        let packed = live
            .run
            .call(&mut live.store, (len, lang_code))
            .map_err(classify)? as u64;
        let (pointer, length) = ((packed >> 32) as usize, (packed & 0xffff_ffff) as usize);
        if length > MAX_OUTPUT_BYTES {
            return Err(KernelError::TooLarge);
        }
        let memory = live.memory.data(&live.store);
        let output = pointer
            .checked_add(length)
            .and_then(|end| memory.get(pointer..end))
            .ok_or_else(|| KernelError::BadOutput("output lies outside memory".to_owned()))?;
        read(output)
    }

    /// Runs the kernel on `text` and hands the raw JSON it returned to `read`,
    /// which must not keep the slice. Returns `read`'s result and the call's
    /// wall time. After an error the instance is discarded and a fresh warmed
    /// one is built from the compiled module before the next run.
    pub fn run_with<R>(
        &self,
        text: &[u8],
        lang: Lang,
        read: impl FnOnce(&[u8]) -> Result<R, KernelError>,
    ) -> Result<(R, Duration), KernelError> {
        if text.len() > self.config.max_input_bytes {
            let error = KernelError::InputTooLarge;
            metrics::run(Duration::ZERO, &Err(&error));
            self.log_error(&error);
            return Err(error);
        }
        let started = Instant::now();
        let mut guard = self.lock();
        let result = match guard.take() {
            Some(mut live) => {
                let result = self.call(&mut live, text, lang, read);
                if result.is_ok() {
                    *guard = Some(live);
                }
                result
            }
            None => match self.fresh() {
                Ok(mut live) => {
                    let result = self.call(&mut live, text, lang, read);
                    if result.is_ok() {
                        *guard = Some(live);
                    }
                    result
                }
                Err(error) => Err(error),
            },
        };
        if result.is_err() && guard.is_none() {
            *guard = self.fresh().ok();
        }
        drop(guard);
        let elapsed = started.elapsed();
        match result {
            Ok(value) => {
                metrics::run(elapsed, &Ok(()));
                Ok((value, elapsed))
            }
            Err(error) => {
                metrics::run(elapsed, &Err(&error));
                self.log_error(&error);
                Err(error)
            }
        }
    }

    /// Runs the kernel on `text` and parses its JSON output into a verdict.
    pub fn run(&self, text: &[u8], lang: Lang) -> RunOutcome {
        let parsed = self.run_with(text, lang, |bytes| {
            serde_json::from_slice::<KernelVerdict>(bytes)
                .map_err(|error| KernelError::BadOutput(error.to_string()))
        });
        match parsed {
            Ok((verdict, elapsed)) => RunOutcome::Ok {
                verdict,
                micros: u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
            },
            Err(error) => RunOutcome::Err(error),
        }
    }

    fn log_error(&self, error: &KernelError) {
        tracing::warn!(kernel_id = self.id, sha256 = %self.sha256, %error, "kernel run failed");
    }
}

fn classify(error: wasmtime::Error) -> KernelError {
    match error.downcast_ref::<Trap>() {
        Some(Trap::Interrupt) => KernelError::Timeout,
        _ => KernelError::Trap(format!("{error:#}")),
    }
}

/// Runs a kernel on a blocking thread, so a call that lasts up to the deadline
/// never stalls the async reactor.
pub async fn run_blocking(kernel: Arc<LoadedKernel>, text: Vec<u8>, lang: Lang) -> RunOutcome {
    match tokio::task::spawn_blocking(move || kernel.run(&text, lang)).await {
        Ok(outcome) => outcome,
        Err(error) => RunOutcome::Err(KernelError::Trap(format!("blocking task failed: {error}"))),
    }
}
