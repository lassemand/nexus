use chrono::{DateTime, NaiveDate, Utc};
use sqlx::PgPool;

use crate::types::{ReportKind, ScheduledReport, TargetStatus};

const TARGET_COLUMNS: &str =
    "id, ticker, exchange_mic, report_kind, fiscal_period, expected_date, \
     expected_time_known, expected_at, signal_event_id, status, prep_due_at, prep_attempts, \
     retry_after, prepared_for_at, created_at, updated_at";

/// A stored report target.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
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
}

/// A note a user added for a report.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::FromRow)]
pub struct AcceptedKernel {
    /// The kernel's id.
    pub id: i64,
    /// Its version for the target.
    pub version: i32,
    /// When it was created.
    pub created_at: DateTime<Utc>,
}

/// A preparation run.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
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
        sqlx::query_as(&format!(
            "SELECT {TARGET_COLUMNS} FROM report_targets
             WHERE status IN ('waiting', 'preparing', 'ready', 'failed')
             ORDER BY expected_at, id"
        ))
        .fetch_all(&self.pool)
        .await
    }

    /// One target by id.
    pub async fn target(&self, id: i64) -> Result<Option<Target>, sqlx::Error> {
        sqlx::query_as(&format!(
            "SELECT {TARGET_COLUMNS} FROM report_targets WHERE id = $1"
        ))
        .bind(id)
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
        let row: Option<(i64,)> = sqlx::query_as(
            "INSERT INTO report_targets
                 (ticker, exchange_mic, report_kind, fiscal_period, expected_date, expected_time_known,
                  expected_at, signal_event_id, status, prep_due_at, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'waiting', $9, $10, $10)
             ON CONFLICT DO NOTHING
             RETURNING id",
        )
        .bind(&report.ticker)
        .bind(&report.exchange_mic)
        .bind(report.report_kind.as_str())
        .bind(&report.fiscal_period)
        .bind(report.expected_date)
        .bind(report.expected_time_known)
        .bind(report.expected_at)
        .bind(report.signal_event_id)
        .bind(prep_due_at)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(id,)| id))
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
        sqlx::query(
            "UPDATE report_targets SET
                 fiscal_period = $2, expected_date = $3, expected_time_known = $4, expected_at = $5,
                 signal_event_id = $6, prep_due_at = $7,
                 status = CASE WHEN $8 THEN 'waiting' ELSE status END,
                 prep_attempts = CASE WHEN $8 THEN 0 ELSE prep_attempts END,
                 retry_after = CASE WHEN $8 THEN NULL ELSE retry_after END,
                 updated_at = $9
             WHERE id = $1",
        )
        .bind(id)
        .bind(&report.fiscal_period)
        .bind(report.expected_date)
        .bind(report.expected_time_known)
        .bind(report.expected_at)
        .bind(report.signal_event_id)
        .bind(prep_due_at)
        .bind(reprepare)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Cancels a waiting or preparing target and closes its running runs.
    pub async fn cancel_target(&self, id: i64, now: DateTime<Utc>) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let cancelled = sqlx::query(
            "UPDATE report_targets SET status = 'cancelled', updated_at = $2
             WHERE id = $1 AND status IN ('waiting', 'preparing')",
        )
        .bind(id)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if cancelled > 0 {
            sqlx::query(
                "UPDATE prep_runs SET outcome = 'failed', finished_at = $2, detail = 'target cancelled'
                 WHERE target_id = $1 AND outcome = 'running'",
            )
            .bind(id)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(cancelled > 0)
    }

    /// How many targets there are in each status.
    pub async fn counts_by_status(&self) -> Result<Vec<(String, i64)>, sqlx::Error> {
        sqlx::query_as("SELECT status, COUNT(*) FROM report_targets GROUP BY status")
            .fetch_all(&self.pool)
            .await
    }

    /// How many targets have a preparation running.
    pub async fn preparing_count(&self) -> Result<i64, sqlx::Error> {
        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM report_targets WHERE status = 'preparing'")
                .fetch_one(&self.pool)
                .await?;
        Ok(count)
    }

    /// Waiting targets whose preparation is due and can still finish in time,
    /// soonest report first.
    pub async fn due_targets(
        &self,
        now: DateTime<Utc>,
        min_lead_secs: f64,
        limit: i64,
    ) -> Result<Vec<Target>, sqlx::Error> {
        sqlx::query_as(&format!(
            "SELECT {TARGET_COLUMNS} FROM report_targets
             WHERE status = 'waiting'
               AND prep_due_at <= $1
               AND (retry_after IS NULL OR retry_after <= $1)
               AND expected_at - $1 >= make_interval(secs => $2)
             ORDER BY expected_at, id
             LIMIT $3"
        ))
        .bind(now)
        .bind(min_lead_secs)
        .bind(limit)
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
        let started: Option<(i32, DateTime<Utc>)> = sqlx::query_as(
            "UPDATE report_targets SET status = 'preparing', prep_attempts = prep_attempts + 1,
                    retry_after = NULL, updated_at = $2
             WHERE id = $1 AND status = 'waiting'
             RETURNING prep_attempts, expected_at",
        )
        .bind(target_id)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((attempt, expected_at)) = started else {
            return Ok(None);
        };
        let (run_id,): (i64,) = sqlx::query_as(
            "INSERT INTO prep_runs (target_id, started_at, attempt, prepared_for_at)
             VALUES ($1, $2, $3, $4) RETURNING id",
        )
        .bind(target_id)
        .bind(now)
        .bind(attempt)
        .bind(expected_at)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(PrepRun {
            id: run_id,
            target_id,
            outcome: "running".to_owned(),
            attempt,
            prepared_for_at: expected_at,
        }))
    }

    /// Records the Job and the upload token's hash a launcher started.
    pub async fn set_run_launch(
        &self,
        run_id: i64,
        job_name: Option<&str>,
        upload_token_hash: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE prep_runs SET job_name = $2, upload_token_hash = $3 WHERE id = $1")
            .bind(run_id)
            .bind(job_name)
            .bind(upload_token_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// One run by id.
    pub async fn run(&self, run_id: i64) -> Result<Option<PrepRun>, sqlx::Error> {
        sqlx::query_as(
            "SELECT id, target_id, outcome, attempt, prepared_for_at FROM prep_runs WHERE id = $1",
        )
        .bind(run_id)
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
        let closed = sqlx::query(
            "UPDATE prep_runs SET outcome = $2, detail = $3, finished_at = $4
             WHERE id = $1 AND outcome = 'running'",
        )
        .bind(run_id)
        .bind(outcome)
        .bind(detail)
        .bind(now)
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
        sqlx::query(
            "UPDATE report_targets SET status = $2, retry_after = $3,
                    prep_attempts = CASE WHEN $4 THEN 0 ELSE prep_attempts END,
                    prepared_for_at = COALESCE($5, prepared_for_at), updated_at = $6
             WHERE id = $1",
        )
        .bind(id)
        .bind(status.as_str())
        .bind(retry_after)
        .bind(reset_attempts)
        .bind(prepared_for_at)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Makes a target's preparation due now if it is not already due.
    pub async fn pull_due_forward(&self, id: i64, now: DateTime<Utc>) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE report_targets SET prep_due_at = LEAST(prep_due_at, $2), updated_at = $2 WHERE id = $1",
        )
        .bind(id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// When the target's latest preparation run finished.
    pub async fn last_run_finished(
        &self,
        target_id: i64,
    ) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
        let (finished,): (Option<DateTime<Utc>>,) =
            sqlx::query_as("SELECT MAX(finished_at) FROM prep_runs WHERE target_id = $1")
                .bind(target_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(finished)
    }

    /// Lets a failed target try once more: back to waiting with one attempt left.
    pub async fn give_one_more_attempt(
        &self,
        id: i64,
        max_attempts: i32,
        now: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE report_targets SET status = 'waiting', prep_attempts = GREATEST($2 - 1, 0),
                    retry_after = NULL, updated_at = $3
             WHERE id = $1 AND status = 'failed'",
        )
        .bind(id)
        .bind(max_attempts)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The target's accepted kernel, if it has one.
    pub async fn accepted_kernel(
        &self,
        target_id: i64,
    ) -> Result<Option<AcceptedKernel>, sqlx::Error> {
        sqlx::query_as(
            "SELECT id, version, created_at FROM kernels WHERE target_id = $1 AND status = 'accepted'",
        )
        .bind(target_id)
        .fetch_optional(&self.pool)
        .await
    }

    /// Stores a note once by its producer id; false when it was already stored.
    pub async fn insert_note(&self, note: &NewNote) -> Result<bool, sqlx::Error> {
        let inserted = sqlx::query(
            "INSERT INTO notes (ticker, exchange_mic, fiscal_period, text, author, created_at, received_at, source_event_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (source_event_id) DO NOTHING",
        )
        .bind(&note.ticker)
        .bind(&note.exchange_mic)
        .bind(&note.fiscal_period)
        .bind(&note.text)
        .bind(&note.author)
        .bind(note.created_at)
        .bind(note.received_at)
        .bind(note.source_event_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(inserted > 0)
    }

    /// The notes that apply to a target: those for its period, and those with
    /// no period that arrived after the company's previous report and before
    /// this one.
    pub async fn notes_for(&self, target: &Target) -> Result<Vec<Note>, sqlx::Error> {
        sqlx::query_as(
            "SELECT n.id, n.ticker, n.exchange_mic, n.fiscal_period, n.text, n.author, n.created_at, n.received_at
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
             ORDER BY n.received_at, n.id",
        )
        .bind(&target.ticker)
        .bind(&target.exchange_mic)
        .bind(&target.fiscal_period)
        .bind(target.expected_at)
        .bind(target.id)
        .fetch_all(&self.pool)
        .await
    }
}
