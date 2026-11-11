use std::str::FromStr;

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::PgPool;

use crate::schedule::{ReportKind, ScheduledReport, TargetIdentity};

/// Where a report target is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetStatus {
    /// Waiting for its preparation to be due.
    Waiting,
    /// A preparation is running.
    Preparing,
    /// A kernel is accepted for it.
    Ready,
    /// Its preparations ran out of attempts or time.
    Failed,
    /// The report was released and decided.
    Decided,
    /// The report was released and skipped.
    Skipped,
    /// The report left the schedule.
    Cancelled,
}

impl TargetStatus {
    /// Every status, for metrics.
    pub const ALL: [TargetStatus; 7] = [
        TargetStatus::Waiting,
        TargetStatus::Preparing,
        TargetStatus::Ready,
        TargetStatus::Failed,
        TargetStatus::Decided,
        TargetStatus::Skipped,
        TargetStatus::Cancelled,
    ];

    /// The value stored in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            TargetStatus::Waiting => "waiting",
            TargetStatus::Preparing => "preparing",
            TargetStatus::Ready => "ready",
            TargetStatus::Failed => "failed",
            TargetStatus::Decided => "decided",
            TargetStatus::Skipped => "skipped",
            TargetStatus::Cancelled => "cancelled",
        }
    }
}

impl FromStr for TargetStatus {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        TargetStatus::ALL
            .into_iter()
            .find(|status| status.as_str() == text)
            .ok_or_else(|| format!("unknown target status `{text}`"))
    }
}

/// A stored report target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The target's id.
    pub id: i64,
    /// The company's ticker.
    pub ticker: String,
    /// The company's exchange.
    pub exchange_mic: String,
    /// `interim` or `annual`.
    pub report_kind: String,
    /// The fiscal period, when known.
    pub fiscal_period: Option<String>,
    /// The scheduled date.
    pub expected_date: NaiveDate,
    /// Whether the calendar gave a time of day.
    pub expected_time_known: bool,
    /// The instant used for scheduling.
    pub expected_at: DateTime<Utc>,
    /// The `special_events` row it last came from.
    pub signal_event_id: i64,
    /// A [`TargetStatus`] as stored.
    pub status: String,
    /// When its preparation becomes due.
    pub prep_due_at: DateTime<Utc>,
    /// Preparations started in the current cycle.
    pub prep_attempts: i32,
    /// A retry waits until this instant.
    pub retry_after: Option<DateTime<Utc>>,
    /// `expected_at` when the ready kernel's preparation started.
    pub prepared_for_at: Option<DateTime<Utc>>,
    /// When the target was created.
    pub created_at: DateTime<Utc>,
    /// When it last changed.
    pub updated_at: DateTime<Utc>,
}

impl Target {
    /// The status, parsed.
    pub fn status(&self) -> Result<TargetStatus, String> {
        self.status.parse()
    }

    /// The report kind, parsed.
    pub fn kind(&self) -> Result<ReportKind, String> {
        self.report_kind.parse()
    }

    /// What decides which scheduled report this target follows.
    pub fn identity(&self) -> Result<TargetIdentity, String> {
        Ok(TargetIdentity {
            id: self.id,
            ticker: self.ticker.clone(),
            exchange_mic: self.exchange_mic.clone(),
            report_kind: self.kind()?,
            fiscal_period: self.fiscal_period.clone(),
            expected_date: self.expected_date,
            signal_event_id: self.signal_event_id,
        })
    }
}

/// A note a user added for a report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// The note's id.
    pub id: i64,
    /// The company's ticker.
    pub ticker: String,
    /// The company's exchange.
    pub exchange_mic: String,
    /// The fiscal period; none means the next report after it arrived.
    pub fiscal_period: Option<String>,
    /// The note.
    pub text: String,
    /// Who wrote it.
    pub author: String,
    /// When the author wrote it.
    pub created_at: DateTime<Utc>,
    /// When analyst stored it.
    pub received_at: DateTime<Utc>,
}

/// A note to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewNote {
    /// The company's ticker.
    pub ticker: String,
    /// The company's exchange.
    pub exchange_mic: String,
    /// The fiscal period; none means the next report.
    pub fiscal_period: Option<String>,
    /// The note.
    pub text: String,
    /// Who wrote it.
    pub author: String,
    /// When the author wrote it.
    pub created_at: DateTime<Utc>,
    /// When analyst received it.
    pub received_at: DateTime<Utc>,
    /// The producer's id for the note.
    pub source_event_id: uuid::Uuid,
}

/// The accepted kernel of a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcceptedKernel {
    /// The kernel's id.
    pub id: i64,
    /// Its version for the target.
    pub version: i32,
    /// When it was created.
    pub created_at: DateTime<Utc>,
}

/// A preparation run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepRun {
    /// The run's id.
    pub id: i64,
    /// The target it prepares.
    pub target_id: i64,
    /// Its outcome as stored.
    pub outcome: String,
    /// Which attempt of the cycle it is.
    pub attempt: i32,
    /// The target's `expected_at` when it started.
    pub prepared_for_at: DateTime<Utc>,
}

/// The analyst's database.
#[derive(Debug, Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    /// Wraps a pool of the analyst database.
    pub fn new(pool: PgPool) -> Store {
        Store { pool }
    }

    /// Targets that can still change: waiting, preparing, ready or failed.
    pub async fn active_targets(&self) -> Result<Vec<Target>, sqlx::Error> {
        sqlx::query_as!(
            Target,
            r#"SELECT id, ticker, exchange_mic, report_kind, fiscal_period, expected_date,
                      expected_time_known, expected_at, signal_event_id, status, prep_due_at,
                      prep_attempts, retry_after, prepared_for_at, created_at, updated_at
               FROM report_targets
               WHERE status IN ('waiting', 'preparing', 'ready', 'failed')
               ORDER BY expected_at, id"#
        )
        .fetch_all(&self.pool)
        .await
    }

    /// One target by id.
    pub async fn target(&self, id: i64) -> Result<Option<Target>, sqlx::Error> {
        sqlx::query_as!(
            Target,
            r#"SELECT id, ticker, exchange_mic, report_kind, fiscal_period, expected_date,
                      expected_time_known, expected_at, signal_event_id, status, prep_due_at,
                      prep_attempts, retry_after, prepared_for_at, created_at, updated_at
               FROM report_targets WHERE id = $1"#,
            id
        )
        .fetch_optional(&self.pool)
        .await
    }

    /// Creates a waiting target for `report`; `None` when an equal one exists.
    pub async fn insert_target(
        &self,
        report: &ScheduledReport,
        prep_due_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<Option<i64>, sqlx::Error> {
        let row = sqlx::query!(
            r#"INSERT INTO report_targets
                   (ticker, exchange_mic, report_kind, fiscal_period, expected_date, expected_time_known,
                    expected_at, signal_event_id, status, prep_due_at, created_at, updated_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'waiting', $9, $10, $10)
               ON CONFLICT DO NOTHING
               RETURNING id"#,
            report.ticker,
            report.exchange_mic,
            report.report_kind.as_str(),
            report.fiscal_period,
            report.expected_date,
            report.expected_time_known,
            report.expected_at,
            report.signal_event_id,
            prep_due_at,
            now
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| row.id))
    }

    /// Follows a report's new date, time, period or row. With `reprepare` the
    /// target goes back to `waiting` with fresh attempts.
    pub async fn update_schedule(
        &self,
        id: i64,
        report: &ScheduledReport,
        prep_due_at: DateTime<Utc>,
        reprepare: bool,
        now: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"UPDATE report_targets SET
                   fiscal_period = $2, expected_date = $3, expected_time_known = $4, expected_at = $5,
                   signal_event_id = $6, prep_due_at = $7,
                   status = CASE WHEN $8 THEN 'waiting' ELSE status END,
                   prep_attempts = CASE WHEN $8 THEN 0 ELSE prep_attempts END,
                   retry_after = CASE WHEN $8 THEN NULL ELSE retry_after END,
                   updated_at = $9
               WHERE id = $1"#,
            id,
            report.fiscal_period,
            report.expected_date,
            report.expected_time_known,
            report.expected_at,
            report.signal_event_id,
            prep_due_at,
            reprepare,
            now
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Cancels a waiting or preparing target and closes its running runs.
    pub async fn cancel_target(&self, id: i64, now: DateTime<Utc>) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let cancelled = sqlx::query!(
            "UPDATE report_targets SET status = 'cancelled', updated_at = $2
             WHERE id = $1 AND status IN ('waiting', 'preparing')",
            id,
            now
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if cancelled > 0 {
            sqlx::query!(
                "UPDATE prep_runs SET outcome = 'failed', finished_at = $2, detail = 'target cancelled'
                 WHERE target_id = $1 AND outcome = 'running'",
                id,
                now
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(cancelled > 0)
    }

    /// How many targets there are in each status.
    pub async fn counts_by_status(&self) -> Result<Vec<(String, i64)>, sqlx::Error> {
        let rows = sqlx::query!(
            r#"SELECT status, COUNT(*) AS "count!" FROM report_targets GROUP BY status"#
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| (row.status, row.count))
            .collect())
    }

    /// How many targets have a preparation running.
    pub async fn preparing_count(&self) -> Result<i64, sqlx::Error> {
        let row = sqlx::query!(
            r#"SELECT COUNT(*) AS "count!" FROM report_targets WHERE status = 'preparing'"#
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.count)
    }

    /// Waiting targets whose preparation is due and can still finish in time,
    /// soonest report first.
    pub async fn due_targets(
        &self,
        now: DateTime<Utc>,
        min_lead_secs: f64,
        limit: i64,
    ) -> Result<Vec<Target>, sqlx::Error> {
        sqlx::query_as!(
            Target,
            r#"SELECT id, ticker, exchange_mic, report_kind, fiscal_period, expected_date,
                      expected_time_known, expected_at, signal_event_id, status, prep_due_at,
                      prep_attempts, retry_after, prepared_for_at, created_at, updated_at
               FROM report_targets
               WHERE status = 'waiting'
                 AND prep_due_at <= $1
                 AND (retry_after IS NULL OR retry_after <= $1)
                 AND expected_at - $1 >= make_interval(secs => $2)
               ORDER BY expected_at, id
               LIMIT $3"#,
            now,
            min_lead_secs,
            limit
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Marks a waiting target `preparing`, counts the attempt and opens a run.
    /// `None` when the target is no longer waiting.
    pub async fn start_preparation(
        &self,
        target_id: i64,
        now: DateTime<Utc>,
    ) -> Result<Option<PrepRun>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let started = sqlx::query!(
            "UPDATE report_targets SET status = 'preparing', prep_attempts = prep_attempts + 1,
                    retry_after = NULL, updated_at = $2
             WHERE id = $1 AND status = 'waiting'
             RETURNING prep_attempts, expected_at",
            target_id,
            now
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(started) = started else {
            return Ok(None);
        };
        let run = sqlx::query!(
            "INSERT INTO prep_runs (target_id, started_at, attempt, prepared_for_at)
             VALUES ($1, $2, $3, $4) RETURNING id",
            target_id,
            now,
            started.prep_attempts,
            started.expected_at
        )
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(PrepRun {
            id: run.id,
            target_id,
            outcome: "running".to_owned(),
            attempt: started.prep_attempts,
            prepared_for_at: started.expected_at,
        }))
    }

    /// Records the Job and the upload token's hash a launcher started.
    pub async fn set_run_launch(
        &self,
        run_id: i64,
        job_name: Option<&str>,
        upload_token_hash: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "UPDATE prep_runs SET job_name = $2, upload_token_hash = $3 WHERE id = $1",
            run_id,
            job_name,
            upload_token_hash
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// One run by id.
    pub async fn run(&self, run_id: i64) -> Result<Option<PrepRun>, sqlx::Error> {
        sqlx::query_as!(
            PrepRun,
            "SELECT id, target_id, outcome, attempt, prepared_for_at FROM prep_runs WHERE id = $1",
            run_id
        )
        .fetch_optional(&self.pool)
        .await
    }

    /// Closes a running run; false when it was not running.
    pub async fn close_run(
        &self,
        run_id: i64,
        outcome: &str,
        detail: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<bool, sqlx::Error> {
        let closed = sqlx::query!(
            "UPDATE prep_runs SET outcome = $2, detail = $3, finished_at = $4
             WHERE id = $1 AND outcome = 'running'",
            run_id,
            outcome,
            detail,
            now
        )
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(closed > 0)
    }

    /// Sets a target's status after a run, with an optional retry instant.
    pub async fn set_target_state(
        &self,
        id: i64,
        status: TargetStatus,
        retry_after: Option<DateTime<Utc>>,
        reset_attempts: bool,
        prepared_for_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "UPDATE report_targets SET status = $2, retry_after = $3,
                    prep_attempts = CASE WHEN $4 THEN 0 ELSE prep_attempts END,
                    prepared_for_at = COALESCE($5, prepared_for_at), updated_at = $6
             WHERE id = $1",
            id,
            status.as_str(),
            retry_after,
            reset_attempts,
            prepared_for_at,
            now
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Makes a target's preparation due now if it is not already due.
    pub async fn pull_due_forward(&self, id: i64, now: DateTime<Utc>) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "UPDATE report_targets SET prep_due_at = LEAST(prep_due_at, $2), updated_at = $2 WHERE id = $1",
            id,
            now
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// When the target's latest preparation run finished.
    pub async fn last_run_finished(
        &self,
        target_id: i64,
    ) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
        let row = sqlx::query!(
            "SELECT MAX(finished_at) AS finished FROM prep_runs WHERE target_id = $1",
            target_id
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.finished)
    }

    /// Lets a failed target try once more: back to waiting with one attempt left.
    pub async fn give_one_more_attempt(
        &self,
        id: i64,
        max_attempts: i32,
        now: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "UPDATE report_targets SET status = 'waiting', prep_attempts = GREATEST($2 - 1, 0),
                    retry_after = NULL, updated_at = $3
             WHERE id = $1 AND status = 'failed'",
            id,
            max_attempts,
            now
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The target's accepted kernel, if it has one.
    pub async fn accepted_kernel(
        &self,
        target_id: i64,
    ) -> Result<Option<AcceptedKernel>, sqlx::Error> {
        let row = sqlx::query!(
            "SELECT id, version, created_at FROM kernels WHERE target_id = $1 AND status = 'accepted'",
            target_id
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| AcceptedKernel {
            id: row.id,
            version: row.version,
            created_at: row.created_at,
        }))
    }

    /// Stores a note once by its producer id; false when it was already stored.
    pub async fn insert_note(&self, note: &NewNote) -> Result<bool, sqlx::Error> {
        let inserted = sqlx::query!(
            "INSERT INTO notes (ticker, exchange_mic, fiscal_period, text, author, created_at, received_at, source_event_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (source_event_id) DO NOTHING",
            note.ticker,
            note.exchange_mic,
            note.fiscal_period,
            note.text,
            note.author,
            note.created_at,
            note.received_at,
            note.source_event_id
        )
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(inserted > 0)
    }

    /// The notes that apply to a target: those for its period, and those with
    /// no period that arrived after the company's previous report and before
    /// this one.
    pub async fn notes_for(&self, target: &Target) -> Result<Vec<Note>, sqlx::Error> {
        sqlx::query_as!(
            Note,
            r#"SELECT n.id, n.ticker, n.exchange_mic, n.fiscal_period, n.text, n.author, n.created_at, n.received_at
               FROM notes n
               WHERE n.ticker = $1 AND n.exchange_mic = $2
                 AND ( ($3::text IS NOT NULL AND n.fiscal_period = $3)
                    OR ( n.fiscal_period IS NULL
                         AND n.received_at < $4
                         AND n.received_at >= COALESCE(
                               (SELECT MAX(p.expected_at) FROM report_targets p
                                 WHERE p.ticker = $1 AND p.exchange_mic = $2
                                   AND p.expected_at < $4 AND p.status <> 'cancelled' AND p.id <> $5),
                               '-infinity'::timestamptz) ) )
               ORDER BY n.received_at, n.id"#,
            target.ticker,
            target.exchange_mic,
            target.fiscal_period,
            target.expected_at,
            target.id
        )
        .fetch_all(&self.pool)
        .await
    }
}
