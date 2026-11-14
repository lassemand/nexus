#![allow(dead_code)]

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use analyst::{
    Clock, LaunchError, LaunchInfo, ManualClock, NoteHandler, PrepLauncher, PrepRequest, Scheduler,
    SchedulerConfig, SignalSchedule, Store, Target,
};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, NaiveTime, TimeZone, Utc};
use model::generated::ReportNote;
use prost::Message;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::sync::Notify;

pub fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, mo, d, h, mi, 0)
        .single()
        .expect("valid instant")
}

pub fn date(y: i32, mo: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, mo, d).expect("valid date")
}

pub fn time(h: u32, m: u32) -> NaiveTime {
    NaiveTime::from_hms_opt(h, m, 0).expect("valid time")
}

pub fn hours(h: u64) -> Duration {
    Duration::from_secs(h * 3600)
}

/// A launcher for tests: it records every request and can be told to fail.
#[derive(Debug, Default)]
pub struct FakeLauncher {
    launched: Mutex<Vec<PrepRequest>>,
    failure: Mutex<Option<String>>,
}

impl FakeLauncher {
    /// A launcher that succeeds.
    pub fn new() -> FakeLauncher {
        FakeLauncher::default()
    }

    /// Every request so far, oldest first.
    pub fn launched(&self) -> Vec<PrepRequest> {
        self.launched
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Makes every following launch fail with `message`; `None` makes them succeed again.
    pub fn fail_with(&self, message: Option<&str>) {
        *self.failure.lock().unwrap_or_else(|p| p.into_inner()) = message.map(str::to_owned);
    }
}

#[async_trait]
impl PrepLauncher for FakeLauncher {
    async fn launch(&self, request: &PrepRequest) -> Result<LaunchInfo, LaunchError> {
        self.launched
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(request.clone());
        match self
            .failure
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        {
            Some(message) => Err(LaunchError(message)),
            None => Ok(LaunchInfo {
                job_name: Some(format!("fake-prep-{}", request.run_id)),
                upload_token_hash: None,
            }),
        }
    }
}

pub struct Settings {
    pub lead: Duration,
    pub min_lead: Duration,
    pub max_concurrent: u32,
    pub max_attempts: u32,
    pub allowlist: Vec<(String, String)>,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            lead: hours(12),
            min_lead: hours(1),
            max_concurrent: 2,
            max_attempts: 3,
            allowlist: Vec::new(),
        }
    }
}

pub struct World {
    pub analyst: PgPool,
    pub signal_writer: PgPool,
    pub clock: Arc<ManualClock>,
    pub launcher: Arc<FakeLauncher>,
    pub scheduler: Scheduler,
    pub store: Store,
    pub signal: SignalSchedule,
    pub wake: Arc<Notify>,
    pub notes: NoteHandler,
}

impl World {
    pub async fn new(pool: PgPool, now: DateTime<Utc>) -> World {
        World::with(pool, now, Settings::default()).await
    }

    pub async fn with(pool: PgPool, now: DateTime<Utc>, settings: Settings) -> World {
        sqlx::query("CREATE SCHEMA signal")
            .execute(&pool)
            .await
            .expect("schema");
        let base = pool.connect_options().as_ref().clone();
        let writer = PgPoolOptions::new()
            .max_connections(3)
            .connect_with(base.clone().options([("search_path", "signal")]))
            .await
            .expect("signal writer");
        let migrations = Path::new(env!("CARGO_MANIFEST_DIR")).join("../signal/migrations");
        sqlx::migrate::Migrator::new(migrations)
            .await
            .expect("signal migrations load")
            .run(&writer)
            .await
            .expect("signal migrations run");
        let reader = PgPoolOptions::new()
            .max_connections(3)
            .connect_with(base.options([
                ("search_path", "signal"),
                ("default_transaction_read_only", "on"),
            ]))
            .await
            .expect("signal reader");
        let clock = Arc::new(ManualClock::new(now));
        let launcher = Arc::new(FakeLauncher::new());
        let store = Store::new(pool.clone());
        let signal = SignalSchedule::new(reader, settings.allowlist.clone());
        let config = SchedulerConfig {
            prep_lead: settings.lead,
            prep_min_lead: settings.min_lead,
            prep_max_concurrent: settings.max_concurrent,
            prep_max_attempts: settings.max_attempts,
            tick: Duration::from_secs(60),
        };
        let scheduler = Scheduler::new(
            store.clone(),
            signal.clone(),
            config,
            clock.clone(),
            launcher.clone(),
        );
        let wake = Arc::new(Notify::new());
        let notes = NoteHandler::new(store.clone(), signal.clone(), clock.clone(), wake.clone());
        World {
            analyst: pool,
            signal_writer: writer,
            clock,
            launcher,
            scheduler,
            store,
            signal,
            wake,
            notes,
        }
    }

    pub async fn add_company(&self, ticker: &str, mic: &str) {
        sqlx::query(
            "INSERT INTO companies (ticker, exchange_mic, currency) VALUES ($1, $2, 'SEK')",
        )
        .bind(ticker)
        .bind(mic)
        .execute(&self.signal_writer)
        .await
        .expect("company");
    }

    pub async fn schedule(
        &self,
        ticker: &str,
        mic: &str,
        day: NaiveDate,
        at: Option<NaiveTime>,
    ) -> i64 {
        self.schedule_with(ticker, mic, day, at, None, "mfn").await
    }

    pub async fn schedule_with(
        &self,
        ticker: &str,
        mic: &str,
        day: NaiveDate,
        at: Option<NaiveTime>,
        period: Option<&str>,
        source: &str,
    ) -> i64 {
        let label = format!("{source}-{day}-{}", period.unwrap_or(""));
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO special_events (ticker, exchange_mic, event_type, occurred_at, status, source, \
                 report_kind, fiscal_period, expected_date, expected_time, calendar_label, calendar_fetched_at) \
             VALUES ($1, $2, 'quarterly_report', ($3::date + COALESCE($4::time, TIME '00:00')) AT TIME ZONE 'Europe/Stockholm', \
                     'scheduled', $5, 'interim', $6, $3, $4, $7, now()) RETURNING id",
        )
        .bind(ticker)
        .bind(mic)
        .bind(day)
        .bind(at)
        .bind(source)
        .bind(period)
        .bind(label)
        .fetch_one(&self.signal_writer)
        .await
        .expect("scheduled row");
        row.0
    }

    pub async fn move_report(&self, id: i64, day: NaiveDate, at: Option<NaiveTime>) {
        sqlx::query(
            "UPDATE special_events SET expected_date = $2, expected_time = $3, \
                 occurred_at = ($2::date + COALESCE($3::time, TIME '00:00')) AT TIME ZONE 'Europe/Stockholm' WHERE id = $1",
        )
        .bind(id)
        .bind(day)
        .bind(at)
        .execute(&self.signal_writer)
        .await
        .expect("moved");
    }

    pub async fn remove_report(&self, id: i64) {
        sqlx::query("DELETE FROM special_events WHERE id = $1")
            .bind(id)
            .execute(&self.signal_writer)
            .await
            .expect("removed");
    }

    pub async fn target(&self, ticker: &str) -> Target {
        let id: (i64,) = sqlx::query_as(
            "SELECT id FROM report_targets WHERE ticker = $1 ORDER BY id DESC LIMIT 1",
        )
        .bind(ticker)
        .fetch_one(&self.analyst)
        .await
        .expect("a target exists");
        self.store
            .target(id.0)
            .await
            .expect("query")
            .expect("found")
    }

    pub async fn target_count(&self) -> i64 {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM report_targets")
            .fetch_one(&self.analyst)
            .await
            .expect("count");
        row.0
    }

    pub async fn make_ready(&self, target_id: i64) {
        let target = self
            .store
            .target(target_id)
            .await
            .expect("query")
            .expect("found");
        let now = self.clock.now();
        sqlx::query(
            "INSERT INTO kernels (target_id, version, source, wasm, sha256, status, created_at, accepted_at) \
             VALUES ($1, 1, 'fn analyze() {}', '\\x0061736d'::bytea, 'abc', 'accepted', $2, $2)",
        )
        .bind(target_id)
        .bind(now)
        .execute(&self.analyst)
        .await
        .expect("kernel");
        self.store
            .set_target_state(
                target_id,
                analyst::TargetStatus::Ready,
                None,
                true,
                Some(target.expected_at),
                now,
            )
            .await
            .expect("ready");
    }

    pub async fn note_count(&self) -> i64 {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM notes")
            .fetch_one(&self.analyst)
            .await
            .expect("count");
        row.0
    }
}

pub fn note_payload(ticker: &str, mic: &str, period: &str, text: &str, id: &str) -> Vec<u8> {
    ReportNote {
        ticker: ticker.into(),
        exchange_mic: mic.into(),
        fiscal_period: period.into(),
        text: text.into(),
        author: "tester".into(),
        created_at_unix_secs: 1_790_000_000,
        note_id: id.into(),
    }
    .encode_to_vec()
}

pub mod rig;
pub mod trading;

pub mod wasm {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::OnceLock;

    fn workspace() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
    }

    /// The WASM build of a kernel crate of the workspace, built once per test binary.
    pub fn bytes(package: &str) -> Vec<u8> {
        static BUILT: OnceLock<()> = OnceLock::new();
        BUILT.get_or_init(|| {
            let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
                .current_dir(workspace())
                .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
                .args(["-p", "verdict-bench-anchored"])
                .status()
                .expect("cargo runs");
            assert!(status.success(), "building the kernels for wasm32 failed");
        });
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace().join("target"));
        let path = target
            .join("wasm32-unknown-unknown/release")
            .join(format!("{}.wasm", package.replace('-', "_")));
        std::fs::read(&path).unwrap_or_else(|error| panic!("reading {path:?}: {error}"))
    }
}
