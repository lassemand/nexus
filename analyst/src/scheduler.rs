use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use thiserror::Error;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use crate::clock::Clock;
use crate::config::{Config, PREP_RETRY_BACKOFF};
use crate::metrics;
use crate::schedule::{match_reports, SignalSchedule};
use crate::store::{PrepRun, Store, Target};
use crate::types::{ScheduledReport, TargetStatus};

const LOOK_AHEAD_EXTRA: Duration = Duration::days(7);

/// What a launcher needs to start a preparation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepRequest {
    /// The report to prepare a kernel for.
    pub target: Target,
    /// The run that tracks this preparation.
    pub run_id: i64,
    /// Which attempt of the cycle this is, from 1.
    pub attempt: i32,
}

/// What a launcher records about the Job it started.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchInfo {
    /// The Kubernetes Job's name.
    pub job_name: Option<String>,
    /// The hash of the token the Job uploads its kernel with.
    pub upload_token_hash: Option<String>,
}

/// A preparation could not be started.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{0}")]
pub struct LaunchError(pub String);

/// Starts the preparation of a kernel for a report. The scheduler decides
/// when; the launcher decides how.
#[async_trait]
pub trait PrepLauncher: Send + Sync {
    /// Starts a preparation. It returns once the preparation has started, not
    /// when it is done; the outcome is reported through
    /// [`Scheduler::finish_run`].
    async fn launch(&self, request: &PrepRequest) -> Result<LaunchInfo, LaunchError>;
}

/// A launcher that starts nothing: it logs the request and reports that it
/// could not start, so no preparation is left running with nobody to finish it.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopLauncher;

#[async_trait]
impl PrepLauncher for NoopLauncher {
    async fn launch(&self, request: &PrepRequest) -> Result<LaunchInfo, LaunchError> {
        info!(
            target_id = request.target.id,
            ticker = %request.target.ticker,
            exchange_mic = %request.target.exchange_mic,
            attempt = request.attempt,
            "no launcher is configured: a preparation is due and was not started"
        );
        Err(LaunchError("no launcher configured".to_owned()))
    }
}

/// How a preparation run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunResult {
    /// A kernel was accepted.
    Succeeded,
    /// The Job failed.
    Failed,
    /// The Job ran out of time.
    TimedOut,
    /// The kernel it uploaded was rejected.
    Rejected,
}

impl RunResult {
    fn outcome(self) -> &'static str {
        match self {
            RunResult::Succeeded => "succeeded",
            RunResult::Failed => "failed",
            RunResult::TimedOut => "timed_out",
            RunResult::Rejected => "rejected",
        }
    }

    fn reason(self) -> &'static str {
        match self {
            RunResult::Succeeded => "succeeded",
            RunResult::Failed => "failed",
            RunResult::TimedOut => "timed_out",
            RunResult::Rejected => "rejected",
        }
    }
}

/// What the scheduler does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerConfig {
    /// How long before a report its preparation starts.
    pub prep_lead: std::time::Duration,
    /// A preparation that cannot finish this long before the report is not started.
    pub prep_min_lead: std::time::Duration,
    /// The most preparations running at once.
    pub prep_max_concurrent: u32,
    /// The most preparations started for one report in one cycle.
    pub prep_max_attempts: u32,
    /// How often the schedule is read.
    pub tick: std::time::Duration,
}

impl From<&Config> for SchedulerConfig {
    fn from(config: &Config) -> SchedulerConfig {
        SchedulerConfig {
            prep_lead: config.prep_lead,
            prep_min_lead: config.prep_min_lead,
            prep_max_concurrent: config.prep_max_concurrent,
            prep_max_attempts: config.prep_max_attempts,
            tick: config.scheduler_tick,
        }
    }
}

/// The scheduler failed to read or write a database.
#[derive(Debug, Error)]
pub enum SchedulerError {
    /// A query failed.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// A stored value is not one the code knows.
    #[error("{0}")]
    Data(String),
    /// The run is unknown or was already finished.
    #[error("run {0} is not running")]
    NotRunning(i64),
}

/// What one scheduler pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PassReport {
    /// Targets created for newly scheduled reports.
    pub created: u32,
    /// Targets whose date, time, period or source row changed.
    pub updated: u32,
    /// Ready targets sent back to be prepared again.
    pub reprepared: u32,
    /// Targets cancelled because their report left the schedule.
    pub cancelled: u32,
    /// Preparations started.
    pub started: u32,
    /// Preparations whose launch failed.
    pub launch_failed: u32,
    /// Updates skipped because they would duplicate another target.
    pub conflicts: u32,
}

fn is_unique_violation(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|db| db.code())
        .is_some_and(|code| code == "23505")
}

fn lead(duration: std::time::Duration) -> Duration {
    Duration::from_std(duration).unwrap_or(Duration::MAX)
}

/// Decides, per upcoming report, when to start preparing a kernel and starts it.
pub struct Scheduler {
    store: Store,
    signal: SignalSchedule,
    config: SchedulerConfig,
    clock: Arc<dyn Clock>,
    launcher: Arc<dyn PrepLauncher>,
    pass_lock: tokio::sync::Mutex<()>,
}

impl Scheduler {
    /// Builds a scheduler over the analyst's database, signal's schedule, a
    /// clock and a launcher.
    pub fn new(
        store: Store,
        signal: SignalSchedule,
        config: SchedulerConfig,
        clock: Arc<dyn Clock>,
        launcher: Arc<dyn PrepLauncher>,
    ) -> Scheduler {
        Scheduler {
            store,
            signal,
            config,
            clock,
            launcher,
            pass_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// One pass: follow signal's schedule, prepare again what date
    /// moves made stale, start what is due, publish the metrics. Passes never
    /// overlap.
    pub async fn pass(&self) -> Result<PassReport, SchedulerError> {
        let _guard = self.pass_lock.lock().await;
        let now = self.clock.now();
        let mut report = PassReport::default();
        self.reconcile(now, &mut report).await?;
        self.start_due(now, &mut report).await?;
        self.publish_metrics(now).await?;
        Ok(report)
    }

    /// Runs passes every tick, and right away when `wake` is notified, until
    /// `shutdown` completes. A failed pass is logged and the loop goes on.
    pub async fn run(&self, wake: Arc<Notify>, shutdown: impl Future<Output = ()>) {
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            match self.pass().await {
                Ok(report) => debug!(?report, "scheduler pass"),
                Err(error) => warn!(%error, "scheduler pass failed"),
            }
            tokio::select! {
                () = &mut shutdown => return,
                () = wake.notified() => {}
                () = tokio::time::sleep(self.config.tick) => {}
            }
        }
    }

    async fn reconcile(
        &self,
        now: DateTime<Utc>,
        report: &mut PassReport,
    ) -> Result<(), SchedulerError> {
        let prep_lead = lead(self.config.prep_lead);
        let horizon = now + prep_lead + LOOK_AHEAD_EXTRA;
        let from = (now - Duration::days(1)).date_naive();
        let to = (horizon + Duration::days(1)).date_naive();
        let reports = self.signal.upcoming(from, to).await?;
        let targets = self.store.active_targets().await?;
        let matching = match_reports(&targets, &reports);
        for &(ti, ri) in &matching.pairs {
            self.follow(&targets[ti], &reports[ri], now, report).await?;
        }
        for &ri in &matching.new_reports {
            let new = &reports[ri];
            if new.expected_at <= now || new.expected_at > horizon {
                continue;
            }
            match self
                .store
                .insert_target(new, new.expected_at - prep_lead, now)
                .await
            {
                Ok(Some(id)) => {
                    report.created += 1;
                    info!(target_id = id, ticker = %new.ticker, exchange_mic = %new.exchange_mic, expected_at = %new.expected_at, "report target created");
                }
                Ok(None) => {}
                Err(error) => return Err(error.into()),
            }
        }
        let unmatched: Vec<&Target> = matching
            .missing_targets
            .iter()
            .map(|&ti| &targets[ti])
            .collect();
        if unmatched.is_empty() {
            return Ok(());
        }
        let ids: Vec<i64> = unmatched.iter().map(|t| t.signal_event_id).collect();
        let still_scheduled: HashMap<i64, ScheduledReport> = self
            .signal
            .by_ids(&ids)
            .await?
            .into_iter()
            .map(|r| (r.signal_event_id, r))
            .collect();
        for target in unmatched {
            match still_scheduled.get(&target.signal_event_id) {
                Some(row) => self.follow(target, row, now, report).await?,
                None => {
                    let open = matches!(
                        target.status(),
                        Ok(TargetStatus::Waiting | TargetStatus::Preparing)
                    );
                    if open && self.store.cancel_target(target.id, now).await? {
                        report.cancelled += 1;
                        info!(target_id = target.id, ticker = %target.ticker, "report left the schedule: target cancelled");
                    }
                }
            }
        }
        Ok(())
    }

    async fn follow(
        &self,
        target: &Target,
        row: &ScheduledReport,
        now: DateTime<Utc>,
        report: &mut PassReport,
    ) -> Result<(), SchedulerError> {
        let period = row
            .fiscal_period
            .clone()
            .or_else(|| target.fiscal_period.clone());
        let unchanged = target.expected_at == row.expected_at
            && target.expected_date == row.expected_date
            && target.expected_time_known == row.expected_time_known
            && target.fiscal_period == period
            && target.signal_event_id == row.signal_event_id;
        if unchanged {
            return Ok(());
        }
        let prep_lead = lead(self.config.prep_lead);
        let status = target.status().map_err(SchedulerError::Data)?;
        let moved_much_later = status == TargetStatus::Ready
            && target
                .prepared_for_at
                .is_some_and(|prepared| row.expected_at - prepared > prep_lead);
        let mut updated = row.clone();
        updated.fiscal_period = period;
        match self
            .store
            .update_schedule(
                target.id,
                &updated,
                row.expected_at - prep_lead,
                moved_much_later,
                now,
            )
            .await
        {
            Ok(()) => {
                report.updated += 1;
                if moved_much_later {
                    report.reprepared += 1;
                }
                info!(target_id = target.id, ticker = %target.ticker, from = %target.expected_at, to = %row.expected_at, moved_much_later, "report target follows its new schedule");
                Ok(())
            }
            Err(error) if is_unique_violation(&error) => {
                report.conflicts += 1;
                warn!(
                    target_id = target.id,
                    "schedule change would duplicate another target, skipped"
                );
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn start_due(
        &self,
        now: DateTime<Utc>,
        report: &mut PassReport,
    ) -> Result<(), SchedulerError> {
        let running = self.store.preparing_count().await?;
        let free = i64::from(self.config.prep_max_concurrent) - running;
        if free <= 0 {
            return Ok(());
        }
        let min_lead_secs = self.config.prep_min_lead.as_secs_f64();
        for target in self.store.due_targets(now, min_lead_secs, free).await? {
            let Some(run) = self.store.start_preparation(target.id, now).await? else {
                continue;
            };
            let request = PrepRequest {
                target,
                run_id: run.id,
                attempt: run.attempt,
            };
            metrics::prep_started();
            match self.launcher.launch(&request).await {
                Ok(info) => {
                    self.store
                        .set_run_launch(
                            run.id,
                            info.job_name.as_deref(),
                            info.upload_token_hash.as_deref(),
                        )
                        .await?;
                    report.started += 1;
                    info!(target_id = request.target.id, run_id = run.id, attempt = run.attempt, ticker = %request.target.ticker, "preparation started");
                }
                Err(error) => {
                    report.launch_failed += 1;
                    warn!(target_id = request.target.id, run_id = run.id, %error, "preparation could not be started");
                    self.store
                        .close_run(run.id, "failed", Some(&error.0), now)
                        .await?;
                    self.fail(&run, "launch", now).await?;
                }
            }
        }
        Ok(())
    }

    /// Records how a preparation run ended and moves its target on: a success
    /// makes it `ready`; a failure retries after a backoff while attempts and
    /// time allow, then gives up.
    pub async fn finish_run(
        &self,
        run_id: i64,
        result: RunResult,
        detail: Option<&str>,
    ) -> Result<(), SchedulerError> {
        let _guard = self.pass_lock.lock().await;
        let now = self.clock.now();
        let run = self
            .store
            .run(run_id)
            .await?
            .ok_or(SchedulerError::NotRunning(run_id))?;
        if !self
            .store
            .close_run(run_id, result.outcome(), detail, now)
            .await?
        {
            return Err(SchedulerError::NotRunning(run_id));
        }
        match result {
            RunResult::Succeeded => {
                self.store
                    .set_target_state(
                        run.target_id,
                        TargetStatus::Ready,
                        None,
                        true,
                        Some(run.prepared_for_at),
                        now,
                    )
                    .await?;
                Ok(())
            }
            other => self.fail(&run, other.reason(), now).await,
        }
    }

    async fn fail(
        &self,
        run: &PrepRun,
        reason: &'static str,
        now: DateTime<Utc>,
    ) -> Result<(), SchedulerError> {
        metrics::prep_failed(reason);
        let Some(target) = self.store.target(run.target_id).await? else {
            return Ok(());
        };
        if target.status().map_err(SchedulerError::Data)? != TargetStatus::Preparing {
            return Ok(());
        }
        let retry_at = now + lead(PREP_RETRY_BACKOFF);
        let attempts_left =
            u32::try_from(target.prep_attempts).unwrap_or(u32::MAX) < self.config.prep_max_attempts;
        let time_left = target.expected_at - retry_at >= lead(self.config.prep_min_lead);
        if attempts_left && time_left {
            self.store
                .set_target_state(
                    target.id,
                    TargetStatus::Waiting,
                    Some(retry_at),
                    false,
                    None,
                    now,
                )
                .await?;
        } else if self.store.accepted_kernel(target.id).await?.is_some() {
            warn!(target_id = target.id, ticker = %target.ticker, "preparing again failed: the accepted kernel stays");
            self.store
                .set_target_state(target.id, TargetStatus::Ready, None, true, None, now)
                .await?;
        } else {
            warn!(target_id = target.id, ticker = %target.ticker, attempts = target.prep_attempts, "no kernel could be prepared: target failed");
            self.store
                .set_target_state(target.id, TargetStatus::Failed, None, false, None, now)
                .await?;
        }
        Ok(())
    }

    async fn publish_metrics(&self, now: DateTime<Utc>) -> Result<(), SchedulerError> {
        let counts: HashMap<String, i64> =
            self.store.counts_by_status().await?.into_iter().collect();
        for status in TargetStatus::ALL {
            metrics::targets(
                status.as_str(),
                counts.get(status.as_str()).copied().unwrap_or(0),
            );
        }
        let soonest = self
            .store
            .active_targets()
            .await?
            .iter()
            .filter(|t| {
                matches!(
                    t.status(),
                    Ok(TargetStatus::Waiting | TargetStatus::Preparing | TargetStatus::Failed)
                )
            })
            .map(|t| t.expected_at)
            .filter(|at| *at > now)
            .min();
        metrics::next_report(
            soonest.map_or(-1.0, |at| (at - now).num_milliseconds() as f64 / 1000.0),
        );
        Ok(())
    }
}
