use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use analyst::{
    DecisionSink, KernelRegistry, KernelRuntime, ManualClock, Processed, Receipt, ReleaseInput,
    ReleaseProcessor, ReportKind, RetryPolicy, RuntimeConfig, Store, TradeHandOff, TradeSignal,
};
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use model::generated::AnalystDecision;
use rdkafka::message::Headers;
use sqlx::PgPool;

pub fn at(h: u32, m: u32, s: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 10, h, m, s)
        .single()
        .expect("valid instant")
}

#[derive(Default)]
pub struct FakeSink {
    pub emitted: Mutex<Vec<(String, AnalystDecision)>>,
    pub headers: Mutex<Vec<Vec<(String, String)>>>,
    pub fail_first: AtomicUsize,
    pub attempts: AtomicUsize,
}

impl FakeSink {
    pub fn emitted(&self) -> Vec<(String, AnalystDecision)> {
        self.emitted.lock().expect("sink lock").clone()
    }
}

#[async_trait]
impl DecisionSink for FakeSink {
    async fn emit(&self, key: &str, decision: &AnalystDecision) -> Result<(), String> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if self.fail_first.load(Ordering::SeqCst) > 0 {
            self.fail_first.fetch_sub(1, Ordering::SeqCst);
            return Err("broker unavailable".to_owned());
        }
        let headers = analyst::trace_headers()
            .iter()
            .map(|header| {
                (
                    header.key.to_owned(),
                    header
                        .value
                        .map(|value| String::from_utf8_lossy(value).into_owned())
                        .unwrap_or_default(),
                )
            })
            .collect();
        self.headers.lock().expect("sink lock").push(headers);
        self.emitted
            .lock()
            .expect("sink lock")
            .push((key.to_owned(), decision.clone()));
        Ok(())
    }
}

#[derive(Default)]
pub struct RecordingHandOff(pub Mutex<Vec<TradeSignal>>);

impl RecordingHandOff {
    pub fn signals(&self) -> Vec<TradeSignal> {
        self.0.lock().expect("hand-off lock").clone()
    }
}

impl TradeHandOff for RecordingHandOff {
    fn hand_off(&self, signal: TradeSignal) {
        self.0.lock().expect("hand-off lock").push(signal);
    }
}

pub struct Rig {
    pub pool: PgPool,
    pub store: Store,
    pub registry: Arc<KernelRegistry>,
    pub clock: Arc<ManualClock>,
    pub sink: Arc<FakeSink>,
    pub hand_off: Arc<RecordingHandOff>,
    pub runtime: Arc<KernelRuntime>,
    pub processor: Arc<ReleaseProcessor>,
    pub kernel: Vec<u8>,
}

impl Rig {
    pub fn new(pool: PgPool) -> Rig {
        let store = Store::new(pool.clone());
        let registry = Arc::new(KernelRegistry::new());
        let clock = Arc::new(ManualClock::new(at(7, 0, 5)));
        let sink = Arc::new(FakeSink::default());
        let hand_off = Arc::new(RecordingHandOff::default());
        let runtime =
            Arc::new(KernelRuntime::new(RuntimeConfig::default()).expect("the engine starts"));
        let processor = Arc::new(
            ReleaseProcessor::new(
                store.clone(),
                Arc::clone(&registry),
                clock.clone(),
                sink.clone(),
                hand_off.clone(),
                Duration::from_secs(15 * 60),
            )
            .with_retry_policy(RetryPolicy {
                start: Duration::from_millis(1),
                max: Duration::from_millis(5),
                non_transient_attempts: 3,
            }),
        );
        Rig {
            pool,
            store,
            registry,
            clock,
            sink,
            hand_off,
            runtime,
            processor,
            kernel: super::wasm::bytes("verdict-bench-anchored"),
        }
    }

    pub async fn target(
        &self,
        ticker: &str,
        period: Option<&str>,
        expected: &str,
        status: &str,
    ) -> i64 {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO report_targets (ticker, exchange_mic, report_kind, fiscal_period, expected_date, \
                 expected_time_known, expected_at, signal_event_id, status, prep_due_at, created_at, updated_at) \
             VALUES ($1, 'XSTO', 'interim', $2, $3::date, true, now(), floor(random() * 1e9)::bigint, $4, now(), now(), now()) \
             RETURNING id",
        )
        .bind(ticker)
        .bind(period)
        .bind(expected)
        .bind(status)
        .fetch_one(&self.pool)
        .await
        .expect("target");
        row.0
    }

    pub async fn accept_kernel(&self, target_id: i64, wasm: &[u8]) -> i64 {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO kernels (target_id, version, source, wasm, sha256, status, created_at, accepted_at) \
             VALUES ($1, 1, 'src', $2, 'abc', 'accepted', now(), now()) RETURNING id",
        )
        .bind(target_id)
        .bind(wasm)
        .fetch_one(&self.pool)
        .await
        .expect("kernel");
        let kernel = self
            .runtime
            .load(u64::try_from(row.0).expect("id"), wasm)
            .expect("the kernel loads");
        self.registry.insert(target_id, Arc::new(kernel));
        row.0
    }

    pub async fn ready_target(&self, ticker: &str, period: &str, expected: &str) -> (i64, i64) {
        let target = self.target(ticker, Some(period), expected, "ready").await;
        let kernel = self.accept_kernel(target, &self.kernel.clone()).await;
        (target, kernel)
    }

    pub fn input(
        &self,
        news_id: &str,
        ticker: &str,
        lang: &str,
        period: Option<&str>,
        text: &str,
    ) -> ReleaseInput {
        ReleaseInput {
            news_id: news_id.to_owned(),
            ticker: ticker.to_owned(),
            exchange_mic: "XSTO".to_owned(),
            lang: lang.to_owned(),
            kind: ReportKind::Interim,
            fiscal_period: period.map(str::to_owned),
            published_at: at(7, 0, 0),
            text: text.to_owned(),
        }
    }

    pub async fn process(&self, input: ReleaseInput) -> Processed {
        let receipt = Receipt::now(self.clock.as_ref());
        self.processor.process(input, receipt).await
    }

    pub async fn count(&self, table: &str) -> i64 {
        let row: (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&self.pool)
            .await
            .expect("count");
        row.0
    }

    pub async fn status(&self, table: &str, id: i64) -> String {
        let row: (String,) = sqlx::query_as(&format!("SELECT status FROM {table} WHERE id = $1"))
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .expect("status");
        row.0
    }
}

/// A kernel that traps when the release text starts with `U`.
pub fn trapping_kernel() -> Vec<u8> {
    let json = r#"{\"decision\":\"abstain\",\"reason\":\"ok\",\"metrics\":[]}"#;
    wat::parse_str(format!(
        r#"(module
  (memory (export "memory") 17)
  (data (i32.const 1024) "{json}")
  (func (export "verdict_input") (param i32) (result i32) (i32.const 65536))
  (func (export "verdict_run") (param i32 i32) (result i64)
    (if (i32.eq (i32.load8_u (i32.const 65536)) (i32.const 85)) (then unreachable))
    (i64.or (i64.shl (i64.const 1024) (i64.const 32)) (i64.const 49))))"#
    ))
    .expect("valid WAT")
}
