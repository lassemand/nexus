//! Review bookkeeping in conductor's database: `review_state` (send-backs per
//! issue) and `review_runs` (the review queued or running for an issue).

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{types::Json, PgConnection, PgPool, Row};

use super::{PrUnderReview, Trigger};

/// One issue's review history.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReviewState {
    pub issue_id: String,
    pub identifier: String,
    /// Reviewer verdicts that sent the issue back to Todo.
    pub send_backs: i32,
    /// Reviews that reached a verdict or failed.
    pub reviews_total: i32,
    pub last_trigger: Option<String>,
    pub last_verdict: Option<String>,
    pub last_review_at: Option<DateTime<Utc>>,
    pub limit_notice_sent_at: Option<DateTime<Utc>>,
}

/// The review queued or running for an issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewRun {
    pub issue_id: String,
    pub identifier: String,
    pub issue_url: String,
    pub group_slug: String,
    pub trigger: Trigger,
    pub review_number: i32,
    /// The pull requests and head SHAs the prompt named.
    pub prs: Vec<PrUnderReview>,
    pub created_at: DateTime<Utc>,
    /// Set when the issue left the review state while this review ran.
    pub cancelled_at: Option<DateTime<Utc>>,
    /// The pull request a `/review` named, `(owner/name, number)`, which the
    /// issue may not list.
    pub extra: Option<(String, u64)>,
}

fn state_from_row(r: &sqlx::postgres::PgRow) -> Result<ReviewState, sqlx::Error> {
    Ok(ReviewState {
        issue_id: r.try_get("issue_id")?,
        identifier: r.try_get("identifier")?,
        send_backs: r.try_get("send_backs")?,
        reviews_total: r.try_get("reviews_total")?,
        last_trigger: r.try_get("last_trigger")?,
        last_verdict: r.try_get("last_verdict")?,
        last_review_at: r.try_get("last_review_at")?,
        limit_notice_sent_at: r.try_get("limit_notice_sent_at")?,
    })
}

fn run_from_row(r: &sqlx::postgres::PgRow) -> Result<ReviewRun, sqlx::Error> {
    let trigger: String = r.try_get("trigger")?;
    let extra_repo: Option<String> = r.try_get("extra_repo")?;
    let extra_pr: Option<i64> = r.try_get("extra_pr")?;
    Ok(ReviewRun {
        issue_id: r.try_get("issue_id")?,
        identifier: r.try_get("identifier")?,
        issue_url: r.try_get("issue_url")?,
        group_slug: r.try_get("group_slug")?,
        trigger: Trigger::parse(&trigger).unwrap_or(Trigger::Automatic),
        review_number: r.try_get("review_number")?,
        prs: r.try_get::<Json<Vec<PrUnderReview>>, _>("prs")?.0,
        created_at: r.try_get("created_at")?,
        cancelled_at: r.try_get("cancelled_at")?,
        extra: extra_repo.zip(extra_pr.and_then(|n| u64::try_from(n).ok())),
    })
}

/// The issue's review history, if it has any.
pub async fn state(pool: &PgPool, issue_id: &str) -> Result<Option<ReviewState>, sqlx::Error> {
    sqlx::query("SELECT * FROM review_state WHERE issue_id = $1")
        .bind(issue_id)
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(state_from_row)
        .transpose()
}

/// Every issue's review history, most recently reviewed first.
pub async fn states(pool: &PgPool) -> Result<Vec<ReviewState>, sqlx::Error> {
    sqlx::query(
        "SELECT * FROM review_state \
         ORDER BY GREATEST(last_review_at, limit_notice_sent_at) DESC NULLS LAST, identifier",
    )
    .fetch_all(pool)
    .await?
    .iter()
    .map(state_from_row)
    .collect()
}

/// Records a review's result. `counted` adds one to `reviews_total` (a
/// discarded review is not counted). Send-backs are counted by
/// [`add_send_back`], when the send-back comment is posted.
pub async fn record_review(
    pool: &PgPool,
    issue_id: &str,
    identifier: &str,
    trigger: Trigger,
    verdict: &str,
    counted: bool,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO review_state \
           (issue_id, identifier, reviews_total, last_trigger, last_verdict, last_review_at) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (issue_id) DO UPDATE SET \
           identifier = EXCLUDED.identifier, \
           reviews_total = review_state.reviews_total + EXCLUDED.reviews_total, \
           last_trigger = EXCLUDED.last_trigger, \
           last_verdict = EXCLUDED.last_verdict, \
           last_review_at = EXCLUDED.last_review_at",
    )
    .bind(issue_id)
    .bind(identifier)
    .bind(i32::from(counted))
    .bind(trigger.as_str())
    .bind(verdict)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Counts one send-back and forgets the limit notice, so the notice is posted
/// again if the issue reaches the limit again.
pub async fn add_send_back(
    pool: &PgPool,
    issue_id: &str,
    identifier: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO review_state (issue_id, identifier, send_backs) VALUES ($1, $2, 1) \
         ON CONFLICT (issue_id) DO UPDATE SET \
           identifier = EXCLUDED.identifier, \
           send_backs = review_state.send_backs + 1, \
           limit_notice_sent_at = NULL",
    )
    .bind(issue_id)
    .bind(identifier)
    .execute(pool)
    .await?;
    Ok(())
}

/// Records that the limit notice was posted.
pub async fn mark_limit_notice(
    pool: &PgPool,
    issue_id: &str,
    identifier: &str,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO review_state (issue_id, identifier, limit_notice_sent_at) VALUES ($1, $2, $3) \
         ON CONFLICT (issue_id) DO UPDATE SET \
           identifier = EXCLUDED.identifier, limit_notice_sent_at = EXCLUDED.limit_notice_sent_at",
    )
    .bind(issue_id)
    .bind(identifier)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Sets an issue's send-backs to 0, by identifier (case-insensitive).
/// Returns how many rows matched.
pub async fn reset(pool: &PgPool, identifier: &str) -> Result<u64, sqlx::Error> {
    Ok(
        sqlx::query("UPDATE review_state SET send_backs = 0, limit_notice_sent_at = NULL WHERE lower(identifier) = lower($1)")
            .bind(identifier)
            .execute(pool)
            .await?
            .rows_affected(),
    )
}

/// Inserts a run, inside the transaction that queues it. Returns `false` when
/// the issue already has one, which is what deduplicates concurrent triggers.
pub async fn insert_run(conn: &mut PgConnection, run: &ReviewRun) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query(
        "INSERT INTO review_runs \
           (issue_id, identifier, issue_url, group_slug, trigger, review_number, prs, created_at, \
            extra_repo, extra_pr) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
         ON CONFLICT (issue_id) DO NOTHING RETURNING issue_id",
    )
    .bind(&run.issue_id)
    .bind(&run.identifier)
    .bind(&run.issue_url)
    .bind(&run.group_slug)
    .bind(run.trigger.as_str())
    .bind(run.review_number)
    .bind(Json(&run.prs))
    .bind(run.created_at)
    .bind(run.extra.as_ref().map(|(repo, _)| repo))
    .bind(run.extra.as_ref().and_then(|(_, n)| i64::try_from(*n).ok()))
    .fetch_optional(conn)
    .await?
    .is_some())
}

/// Points a review set's worktree at the commit under review, replacing any
/// earlier value: each review starts from a freshly created set.
pub async fn set_worktree_ref(
    conn: &mut PgConnection,
    slug: &str,
    worktree_ref: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE session_groups SET worktree_ref = $2 WHERE slug = $1")
        .bind(slug)
        .bind(worktree_ref)
        .execute(conn)
        .await?;
    Ok(())
}

/// The run for an issue, if one is queued or running.
pub async fn run(pool: &PgPool, issue_id: &str) -> Result<Option<ReviewRun>, sqlx::Error> {
    sqlx::query("SELECT * FROM review_runs WHERE issue_id = $1")
        .bind(issue_id)
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(run_from_row)
        .transpose()
}

/// Every queued or running review, oldest first.
pub async fn runs(pool: &PgPool) -> Result<Vec<ReviewRun>, sqlx::Error> {
    sqlx::query("SELECT * FROM review_runs ORDER BY created_at, issue_id")
        .fetch_all(pool)
        .await?
        .iter()
        .map(run_from_row)
        .collect()
}

/// Marks an issue's run as cancelled. Returns whether there was one.
pub async fn cancel_run(
    pool: &PgPool,
    issue_id: &str,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE review_runs SET cancelled_at = $2 WHERE issue_id = $1 AND cancelled_at IS NULL",
    )
    .bind(issue_id)
    .bind(now)
    .execute(pool)
    .await?
    .rows_affected()
        > 0)
}

/// Forgets an issue's run once its verdict is handled.
pub async fn delete_run(pool: &PgPool, issue_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM review_runs WHERE issue_id = $1")
        .bind(issue_id)
        .execute(pool)
        .await?;
    Ok(())
}
