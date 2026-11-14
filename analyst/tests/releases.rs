//! The release path: a report release in, one decision out, against a real
//! Postgres and the real `bench-anchored` kernel built for WASM.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use analyst::{
    classify, preload, with_retry, Classified, DecisionKind, DecisionSink, KernelRegistry,
    KernelRuntime, ManualClock, Processed, Receipt, ReleaseInput, ReleaseProcessor, ReportKind,
    RetryPolicy, RuntimeConfig, Store, TradeHandOff, TradeSignal,
};
use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, TimeZone, Utc};
use model::generated::{AnalystDecision, SpecialEvent};
use sqlx::PgPool;

#[allow(dead_code)]
#[path = "../../verdict/kernels/bench-common/fixtures.rs"]
mod fixtures;

fn at(h: u32, m: u32, s: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 10, h, m, s)
        .single()
        .expect("valid instant")
}

#[derive(Default)]
struct FakeSink {
    emitted: Mutex<Vec<(String, AnalystDecision)>>,
    fail_first: AtomicUsize,
    attempts: AtomicUsize,
}

impl FakeSink {
    fn emitted(&self) -> Vec<(String, AnalystDecision)> {
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
        self.emitted
            .lock()
            .expect("sink lock")
            .push((key.to_owned(), decision.clone()));
        Ok(())
    }
}

#[derive(Default)]
struct RecordingHandOff(Mutex<Vec<TradeSignal>>);

impl RecordingHandOff {
    fn signals(&self) -> Vec<TradeSignal> {
        self.0.lock().expect("hand-off lock").clone()
    }
}

impl TradeHandOff for RecordingHandOff {
    fn hand_off(&self, signal: TradeSignal) {
        self.0.lock().expect("hand-off lock").push(signal);
    }
}

struct Rig {
    pool: PgPool,
    store: Store,
    registry: Arc<KernelRegistry>,
    clock: Arc<ManualClock>,
    sink: Arc<FakeSink>,
    hand_off: Arc<RecordingHandOff>,
    runtime: Arc<KernelRuntime>,
    processor: Arc<ReleaseProcessor>,
    kernel: Vec<u8>,
}

impl Rig {
    fn new(pool: PgPool) -> Rig {
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
            kernel: common::wasm::bytes("verdict-bench-anchored"),
        }
    }

    async fn target(
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

    async fn accept_kernel(&self, target_id: i64, wasm: &[u8]) -> i64 {
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

    async fn ready_target(&self, ticker: &str, period: &str, expected: &str) -> (i64, i64) {
        let target = self.target(ticker, Some(period), expected, "ready").await;
        let kernel = self.accept_kernel(target, &self.kernel.clone()).await;
        (target, kernel)
    }

    fn input(
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

    async fn process(&self, input: ReleaseInput) -> Processed {
        let receipt = Receipt::now(self.clock.as_ref());
        self.processor.process(input, receipt).await
    }

    async fn count(&self, table: &str) -> i64 {
        let row: (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&self.pool)
            .await
            .expect("count");
        row.0
    }

    async fn status(&self, table: &str, id: i64) -> String {
        let row: (String,) = sqlx::query_as(&format!("SELECT status FROM {table} WHERE id = $1"))
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .expect("status");
        row.0
    }
}

fn q3_en() -> String {
    fixtures::fixtures()[0].text.to_owned()
}

fn q3_no() -> String {
    fixtures::fixtures()[1].text.to_owned()
}

fn kind_of(processed: &Processed) -> DecisionKind {
    match processed {
        Processed::Decided { kind, .. } => *kind,
        other => panic!("expected a decision, got {other:?}"),
    }
}

#[sqlx::test]
async fn a_release_with_a_kernel_gives_one_decision_one_event_and_settles_the_report(pool: PgPool) {
    let rig = Rig::new(pool);
    let (target, kernel) = rig.ready_target("AAA", "2026-Q3", "2026-11-10").await;

    let processed = rig
        .process(rig.input("news-en", "AAA", "en", Some("2026-Q3"), &q3_en()))
        .await;
    assert_eq!(kind_of(&processed), DecisionKind::Buy);

    assert_eq!(rig.count("decisions").await, 1);
    let stored = rig
        .store
        .decision_by_news_id("news-en")
        .await
        .expect("query")
        .expect("stored");
    assert_eq!(stored.kind(), Ok(DecisionKind::Buy));
    assert_eq!(
        (stored.target_id, stored.kernel_id),
        (Some(target), Some(kernel))
    );
    assert_eq!(stored.reason.as_deref(), Some("group and farming EBIT up"));
    assert_eq!(stored.metrics.0.len(), 3);
    assert!(stored.kernel_micros.is_some());
    assert_eq!(stored.published_at, at(7, 0, 0));

    let emitted = rig.sink.emitted();
    assert_eq!(emitted.len(), 1);
    assert_eq!(emitted[0].0, "AAA:XSTO");
    assert_eq!(emitted[0].1.decision, "buy");
    assert_eq!(emitted[0].1.news_id, "news-en");
    assert_eq!(emitted[0].1.target_id, target);
    assert_eq!(emitted[0].1.metrics.len(), 3);
    assert_eq!(emitted[0].1.metrics[0].current, "459");
    assert_eq!(
        emitted[0].1.published_at_unix_millis,
        at(7, 0, 0).timestamp_millis()
    );

    let signals = rig.hand_off.signals();
    assert_eq!(signals.len(), 1);
    assert_eq!(
        (signals[0].decision, signals[0].baseline),
        (DecisionKind::Buy, false)
    );

    assert_eq!(rig.status("report_targets", target).await, "decided");
    assert_eq!(rig.status("kernels", kernel).await, "spent");
}

#[sqlx::test]
async fn the_same_release_again_changes_nothing(pool: PgPool) {
    let rig = Rig::new(pool);
    rig.ready_target("AAA", "2026-Q3", "2026-11-10").await;
    let input = rig.input("news-en", "AAA", "en", Some("2026-Q3"), &q3_en());
    rig.process(input.clone()).await;

    assert_eq!(rig.process(input).await, Processed::Duplicate);
    assert_eq!(rig.count("decisions").await, 1);
    assert_eq!(rig.sink.emitted().len(), 1, "no second event");
    assert_eq!(rig.hand_off.signals().len(), 1, "no second trade");
}

#[sqlx::test]
async fn the_other_language_25_seconds_later_is_a_twin(pool: PgPool) {
    let rig = Rig::new(pool);
    let (target, _) = rig.ready_target("AAA", "2026-Q3", "2026-11-10").await;
    rig.process(rig.input("news-en", "AAA", "en", Some("2026-Q3"), &q3_en()))
        .await;

    rig.clock.advance(ChronoDuration::seconds(25));
    let mut norwegian = rig.input("news-no", "AAA", "no", Some("2026-Q3"), &q3_no());
    norwegian.published_at = at(7, 0, 25);
    let processed = rig.process(norwegian).await;
    assert_eq!(kind_of(&processed), DecisionKind::TwinIgnored);

    let stored = rig
        .store
        .decision_by_news_id("news-no")
        .await
        .expect("query")
        .expect("stored");
    assert_eq!(stored.kind(), Ok(DecisionKind::TwinIgnored));
    assert_eq!(stored.target_id, Some(target));
    assert_eq!(rig.hand_off.signals().len(), 1, "a twin is not traded");
    assert_eq!(rig.status("report_targets", target).await, "decided");
    assert_eq!(rig.sink.emitted().len(), 2);
}

#[sqlx::test]
async fn a_release_with_no_target_is_skipped_and_handed_on_as_a_baseline(pool: PgPool) {
    let rig = Rig::new(pool);
    let processed = rig
        .process(rig.input("early", "AAA", "en", Some("2026-Q3"), &q3_en()))
        .await;
    assert_eq!(kind_of(&processed), DecisionKind::SkippedNoKernel);
    let stored = rig
        .store
        .decision_by_news_id("early")
        .await
        .expect("query")
        .expect("stored");
    assert_eq!((stored.target_id, stored.kernel_id), (None, None));
    let signals = rig.hand_off.signals();
    assert_eq!(signals.len(), 1);
    assert!(signals[0].baseline, "simulated, but marked as a baseline");
}

#[sqlx::test]
async fn a_release_whose_target_is_not_ready_is_skipped(pool: PgPool) {
    let rig = Rig::new(pool);
    let waiting = rig
        .target("AAA", Some("2026-Q3"), "2026-11-10", "waiting")
        .await;
    let processed = rig
        .process(rig.input("n", "AAA", "en", Some("2026-Q3"), &q3_en()))
        .await;
    assert_eq!(kind_of(&processed), DecisionKind::SkippedNoKernel);
    let stored = rig
        .store
        .decision_by_news_id("n")
        .await
        .expect("query")
        .expect("stored");
    assert_eq!(stored.target_id, Some(waiting));
    assert_eq!(rig.status("report_targets", waiting).await, "skipped");
}

#[sqlx::test]
async fn a_release_older_than_the_limit_is_stale_and_never_traded(pool: PgPool) {
    let rig = Rig::new(pool);
    let (target, kernel) = rig.ready_target("AAA", "2026-Q3", "2026-11-10").await;
    rig.clock.set(at(7, 20, 0));
    let processed = rig
        .process(rig.input("old", "AAA", "en", Some("2026-Q3"), &q3_en()))
        .await;
    assert_eq!(kind_of(&processed), DecisionKind::Stale);
    assert!(rig.hand_off.signals().is_empty(), "stale is never traded");
    assert_eq!(rig.status("report_targets", target).await, "skipped");
    assert_eq!(rig.status("kernels", kernel).await, "spent");
    assert_eq!(
        rig.sink.emitted().len(),
        1,
        "but it is recorded and emitted"
    );
}

#[sqlx::test]
async fn a_release_just_inside_the_limit_is_decided(pool: PgPool) {
    let rig = Rig::new(pool);
    rig.ready_target("AAA", "2026-Q3", "2026-11-10").await;
    rig.clock.set(at(7, 14, 59));
    let processed = rig
        .process(rig.input("fresh", "AAA", "en", Some("2026-Q3"), &q3_en()))
        .await;
    assert_eq!(kind_of(&processed), DecisionKind::Buy);
}

fn trapping_kernel() -> Vec<u8> {
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

#[sqlx::test]
async fn a_kernel_that_traps_gives_a_kernel_error_and_the_service_goes_on(pool: PgPool) {
    let rig = Rig::new(pool);
    let target = rig
        .target("AAA", Some("2026-Q3"), "2026-11-10", "ready")
        .await;
    rig.accept_kernel(target, &trapping_kernel()).await;
    let processed = rig
        .process(rig.input("trap", "AAA", "en", Some("2026-Q3"), "Unreachable text"))
        .await;
    assert_eq!(kind_of(&processed), DecisionKind::KernelError);
    let stored = rig
        .store
        .decision_by_news_id("trap")
        .await
        .expect("query")
        .expect("stored");
    assert_eq!(stored.error.as_deref(), Some("trap"));
    assert!(
        rig.hand_off.signals().is_empty(),
        "a kernel error is not traded"
    );
    assert_eq!(rig.status("report_targets", target).await, "decided");

    let (other, _) = rig.ready_target("BBB", "2026-Q3", "2026-11-10").await;
    let next = rig
        .process(rig.input("next", "BBB", "en", Some("2026-Q3"), &q3_en()))
        .await;
    assert_eq!(kind_of(&next), DecisionKind::Buy);
    assert_eq!(rig.status("report_targets", other).await, "decided");
}

#[sqlx::test]
async fn the_fiscal_period_beats_the_nearest_date(pool: PgPool) {
    let rig = Rig::new(pool);
    let (q2, _) = rig.ready_target("AAA", "2026-Q2", "2026-11-10").await;
    let (q3, _) = rig.ready_target("AAA", "2026-Q3", "2026-11-24").await;
    let processed = rig
        .process(rig.input("n", "AAA", "en", Some("2026-Q3"), &q3_en()))
        .await;
    assert_eq!(kind_of(&processed), DecisionKind::Buy);
    let stored = rig
        .store
        .decision_by_news_id("n")
        .await
        .expect("query")
        .expect("stored");
    assert_eq!(
        stored.target_id,
        Some(q3),
        "the Q3 target, though Q2's date is the nearer"
    );
    assert_eq!(rig.status("report_targets", q2).await, "ready");
}

#[sqlx::test]
async fn a_report_two_weeks_before_its_date_is_skipped(pool: PgPool) {
    let rig = Rig::new(pool);
    let (target, _) = rig.ready_target("AAA", "2026-Q3", "2026-11-24").await;
    let processed = rig
        .process(rig.input("n", "AAA", "en", None, &q3_en()))
        .await;
    assert_eq!(kind_of(&processed), DecisionKind::SkippedNoKernel);
    assert_eq!(
        rig.status("report_targets", target).await,
        "ready",
        "the target is untouched"
    );
}

#[sqlx::test]
async fn two_versions_at_once_give_one_trade(pool: PgPool) {
    let rig = Rig::new(pool);
    rig.ready_target("AAA", "2026-Q3", "2026-11-10").await;
    let english = rig.input("news-en", "AAA", "en", Some("2026-Q3"), &q3_en());
    let norwegian = rig.input("news-no", "AAA", "no", Some("2026-Q3"), &q3_no());
    let (a, b) = tokio::join!(rig.process(english), rig.process(norwegian));
    let mut kinds = [kind_of(&a), kind_of(&b)];
    kinds.sort_by_key(|kind| kind.as_str());
    assert_eq!(kinds, [DecisionKind::Buy, DecisionKind::TwinIgnored]);
    assert_eq!(rig.hand_off.signals().len(), 1, "exactly one trade");
    assert_eq!(rig.count("decisions").await, 2);
}

#[sqlx::test]
async fn two_unscheduled_versions_at_once_give_one_baseline(pool: PgPool) {
    let rig = Rig::new(pool);
    let english = rig.input("news-en", "AAA", "en", None, &q3_en());
    let norwegian = rig.input("news-no", "AAA", "no", None, &q3_no());
    let (a, b) = tokio::join!(rig.process(english), rig.process(norwegian));
    let mut kinds = [kind_of(&a), kind_of(&b)];
    kinds.sort_by_key(|kind| kind.as_str());
    assert_eq!(
        kinds,
        [DecisionKind::SkippedNoKernel, DecisionKind::TwinIgnored]
    );
    assert_eq!(rig.hand_off.signals().len(), 1);
}

#[sqlx::test]
async fn an_event_that_cannot_be_emitted_yet_is_retried_until_it_goes_out(pool: PgPool) {
    let rig = Rig::new(pool);
    rig.ready_target("AAA", "2026-Q3", "2026-11-10").await;
    rig.sink.fail_first.store(3, Ordering::SeqCst);
    let processed = rig
        .process(rig.input("n", "AAA", "en", Some("2026-Q3"), &q3_en()))
        .await;
    assert_eq!(kind_of(&processed), DecisionKind::Buy);
    assert_eq!(rig.sink.attempts.load(Ordering::SeqCst), 4);
    assert_eq!(rig.sink.emitted().len(), 1);
    assert_eq!(
        rig.count("decisions").await,
        1,
        "the decision was durable before the event"
    );
}

#[sqlx::test]
async fn accepted_kernels_of_open_reports_are_loaded_at_startup(pool: PgPool) {
    let rig = Rig::new(pool);
    let open = rig
        .target("AAA", Some("2026-Q3"), "2026-11-10", "ready")
        .await;
    let decided = rig
        .target("BBB", Some("2026-Q3"), "2026-11-10", "decided")
        .await;
    let wasm = rig.kernel.clone();
    for target in [open, decided] {
        sqlx::query(
            "INSERT INTO kernels (target_id, version, source, wasm, sha256, status, created_at) \
             VALUES ($1, 1, 'src', $2, 'abc', 'accepted', now())",
        )
        .bind(target)
        .bind(&wasm)
        .execute(&rig.pool)
        .await
        .expect("kernel");
    }
    let broken = rig
        .target("CCC", Some("2026-Q3"), "2026-11-10", "ready")
        .await;
    sqlx::query(
        "INSERT INTO kernels (target_id, version, source, wasm, sha256, status, created_at) \
         VALUES ($1, 1, 'src', '\\x0061736d'::bytea, 'abc', 'accepted', now())",
    )
    .bind(broken)
    .execute(&rig.pool)
    .await
    .expect("kernel");

    let fresh = Arc::new(KernelRegistry::new());
    let report = preload(&rig.store, &rig.runtime, &fresh)
        .await
        .expect("preload");
    assert_eq!((report.loaded, report.failed), (1, 1));
    assert!(fresh.get(open).is_some());
    assert!(
        fresh.get(decided).is_none(),
        "a decided report's kernel is not needed"
    );
    assert!(fresh.get(broken).is_none());
}

#[sqlx::test]
async fn the_latency_of_a_decision_stays_far_under_the_budget(pool: PgPool) {
    let rig = Rig::new(pool);
    let companies: Vec<String> = (0..20).map(|i| format!("C{i:02}")).collect();
    for ticker in &companies {
        rig.ready_target(ticker, "2026-Q3", "2026-11-10").await;
    }
    let mut times = Vec::new();
    let mut n = 0;
    for ticker in &companies {
        let mut batch = vec![rig.input(
            &format!("{ticker}-en"),
            ticker,
            "en",
            Some("2026-Q3"),
            &q3_en(),
        )];
        let mut twin = rig.input(
            &format!("{ticker}-no"),
            ticker,
            "no",
            Some("2026-Q3"),
            &q3_no(),
        );
        twin.published_at = at(7, 0, 25);
        batch.push(twin);
        for i in 0..8 {
            let mut other = rig.input(
                &format!("{ticker}-x{i}"),
                ticker,
                "en",
                Some(&format!("2027-Q{}", i % 4 + 1)),
                &q3_en(),
            );
            other.published_at = at(7, 0, 0) + ChronoDuration::days(60 + i64::from(i));
            batch.push(other);
        }
        for input in batch {
            n += 1;
            rig.clock.set(input.published_at.max(at(7, 0, 5)));
            match rig.process(input).await {
                Processed::Decided { to_hand_off, .. } => times.push(to_hand_off),
                other => panic!("expected a decision, got {other:?}"),
            }
        }
    }
    assert_eq!(n, 200);
    times.sort();
    let p50 = times[times.len() / 2];
    let p99 = times[times.len() * 99 / 100];
    eprintln!(
        "received -> handed on, 200 releases: p50 {p50:?} p99 {p99:?} max {:?}",
        times[times.len() - 1]
    );
    assert!(p99 < Duration::from_millis(10), "p99 was {p99:?}");
    assert_eq!(rig.count("decisions").await, 200);
    let started = Instant::now();
    let _ = started;
}

#[tokio::test(start_paused = true)]
async fn a_kernel_instance_is_dropped_an_hour_after_the_decision() {
    let runtime = KernelRuntime::new(RuntimeConfig::default()).expect("the engine starts");
    let wasm = trapping_kernel();
    let kernel = Arc::new(runtime.load(1, &wasm).expect("loads"));
    let registry = Arc::new(KernelRegistry::new());
    registry.insert(7, Arc::clone(&kernel));
    registry.schedule_drop(7, &kernel, Duration::from_secs(3600));
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(3599)).await;
    tokio::task::yield_now().await;
    assert!(registry.get(7).is_some(), "twins may still arrive");
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    assert!(registry.get(7).is_none());

    let replaced = Arc::new(runtime.load(2, &wasm).expect("loads"));
    registry.insert(8, Arc::clone(&kernel));
    registry.schedule_drop(8, &kernel, Duration::from_secs(10));
    tokio::task::yield_now().await;
    registry.insert(8, replaced);
    tokio::time::advance(Duration::from_secs(11)).await;
    tokio::task::yield_now().await;
    assert!(registry.get(8).is_some(), "a kernel that replaced it stays");
}

fn event() -> SpecialEvent {
    SpecialEvent {
        ticker: "AAA".into(),
        event_type: "quarterly_report".into(),
        occurred_at_unix_secs: at(7, 0, 0).timestamp(),
        description: "AAA: Interim report".into(),
        exchange_mic: "XSTO".into(),
        source: "mfn".into(),
        source_id: "6f9619ff-8b86-d011-b42d-00cf4fc964ff".into(),
        lang: "en".into(),
        report_kind: "interim".into(),
        fiscal_period: "2026-Q3".into(),
        text: "AAA: Interim report\n\nNet sales were 5 (4).".into(),
        ..SpecialEvent::default()
    }
}

#[test]
fn a_released_report_passes_the_filter() {
    match classify(&event()) {
        Classified::Process(input) => {
            assert_eq!(input.news_id, "6f9619ff-8b86-d011-b42d-00cf4fc964ff");
            assert_eq!(
                (input.ticker.as_str(), input.exchange_mic.as_str()),
                ("AAA", "XSTO")
            );
            assert_eq!(input.kind, ReportKind::Interim);
            assert_eq!(input.fiscal_period.as_deref(), Some("2026-Q3"));
            assert_eq!(input.published_at, at(7, 0, 0));
        }
        other => panic!("expected to process, got {other:?}"),
    }
    let mut annual = event();
    annual.event_type = "annual_report".into();
    annual.fiscal_period = String::new();
    match classify(&annual) {
        Classified::Process(input) => {
            assert_eq!(input.kind, ReportKind::Annual);
            assert_eq!(input.fiscal_period, None);
        }
        other => panic!("expected to process, got {other:?}"),
    }
}

#[test]
fn everything_else_is_ignored_with_a_reason() {
    let ignored = |change: &dyn Fn(&mut SpecialEvent)| {
        let mut e = event();
        change(&mut e);
        match classify(&e) {
            Classified::Ignore(reason) => reason,
            other => panic!("expected to ignore, got {other:?}"),
        }
    };
    assert_eq!(
        ignored(&|e| e.event_type = "report_correction".into()),
        "event_type"
    );
    assert_eq!(
        ignored(&|e| e.event_type = "report_calendar".into()),
        "event_type"
    );
    assert_eq!(
        ignored(&|e| e.event_type = "company_announcement".into()),
        "event_type"
    );
    assert_eq!(ignored(&|e| e.event_type = "earnings".into()), "event_type");
    assert_eq!(ignored(&|e| e.source = String::new()), "source");
    assert_eq!(ignored(&|e| e.source = "nasdaq".into()), "source");
    assert_eq!(ignored(&|e| e.is_correction = true), "correction");
    assert_eq!(ignored(&|e| e.text = String::new()), "no_text");
    assert_eq!(ignored(&|e| e.text = "  \n ".into()), "no_text");
    assert_eq!(ignored(&|e| e.source_id = String::new()), "no_news_id");
    assert_eq!(ignored(&|e| e.ticker = String::new()), "invalid");
    assert_eq!(ignored(&|e| e.exchange_mic = String::new()), "invalid");
    assert_eq!(ignored(&|e| e.occurred_at_unix_secs = 0), "invalid");
}

#[tokio::test]
async fn a_failing_database_call_is_retried_and_a_hopeless_one_gives_up() {
    let policy = RetryPolicy {
        start: Duration::from_millis(1),
        max: Duration::from_millis(2),
        non_transient_attempts: 3,
    };
    let calls = AtomicUsize::new(0);
    let value = with_retry("test", policy, || async {
        if calls.fetch_add(1, Ordering::SeqCst) < 7 {
            Err(sqlx::Error::PoolTimedOut)
        } else {
            Ok(42)
        }
    })
    .await
    .expect("transient errors are retried until the call works");
    assert_eq!((value, calls.load(Ordering::SeqCst)), (42, 8));

    let calls = AtomicUsize::new(0);
    let result: Result<(), sqlx::Error> = with_retry("test", policy, || async {
        calls.fetch_add(1, Ordering::SeqCst);
        Err(sqlx::Error::RowNotFound)
    })
    .await;
    assert!(result.is_err());
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "a failure that does not pass is tried 3 times"
    );
}
