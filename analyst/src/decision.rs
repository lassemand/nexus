use std::str::FromStr;

use chrono::{DateTime, Utc};
use sqlx::types::Json;

use crate::store::Store;
use crate::types::TargetStatus;
use crate::verdict::KernelMetric;

/// What the analyst decided about one release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DecisionKind {
    /// The kernel says buy.
    Buy,
    /// The kernel says do not buy.
    NoBuy,
    /// The kernel cannot decide.
    Abstain,
    /// No prepared kernel was ready for the release.
    SkippedNoKernel,
    /// The kernel failed to run: a trap, a timeout, a bad output.
    KernelError,
    /// The release is older than `MAX_RELEASE_AGE`; never traded.
    Stale,
    /// Another language version of the same report already decided.
    TwinIgnored,
}

impl DecisionKind {
    /// Every kind, for metrics.
    pub const ALL: [DecisionKind; 7] = [
        DecisionKind::Buy,
        DecisionKind::NoBuy,
        DecisionKind::Abstain,
        DecisionKind::SkippedNoKernel,
        DecisionKind::KernelError,
        DecisionKind::Stale,
        DecisionKind::TwinIgnored,
    ];

    /// The value stored in `decisions.decision` and sent on `analyst.decisions`.
    pub fn as_str(self) -> &'static str {
        match self {
            DecisionKind::Buy => "buy",
            DecisionKind::NoBuy => "no_buy",
            DecisionKind::Abstain => "abstain",
            DecisionKind::SkippedNoKernel => "skipped_no_kernel",
            DecisionKind::KernelError => "kernel_error",
            DecisionKind::Stale => "stale",
            DecisionKind::TwinIgnored => "twin_ignored",
        }
    }

    /// True for a decision that settles its report: every kind but a twin.
    pub fn settles_report(self) -> bool {
        self != DecisionKind::TwinIgnored
    }

    /// The status the report's target gets once this is decided, `None` for a
    /// twin, which changes nothing. A kernel's own verdict, or its error, makes
    /// the target `decided`; a release that was too old or had no kernel ready
    /// makes it `skipped`.
    pub fn target_status(self) -> Option<TargetStatus> {
        match self {
            DecisionKind::Buy
            | DecisionKind::NoBuy
            | DecisionKind::Abstain
            | DecisionKind::KernelError => Some(TargetStatus::Decided),
            DecisionKind::SkippedNoKernel | DecisionKind::Stale => Some(TargetStatus::Skipped),
            DecisionKind::TwinIgnored => None,
        }
    }
}

impl From<verdict_sdk::Decision> for DecisionKind {
    fn from(decision: verdict_sdk::Decision) -> DecisionKind {
        match decision {
            verdict_sdk::Decision::Buy => DecisionKind::Buy,
            verdict_sdk::Decision::NoBuy => DecisionKind::NoBuy,
            verdict_sdk::Decision::Abstain => DecisionKind::Abstain,
        }
    }
}

impl FromStr for DecisionKind {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        DecisionKind::ALL
            .into_iter()
            .find(|kind| kind.as_str() == text)
            .ok_or_else(|| format!("unknown decision `{text}`"))
    }
}

/// A decision to record.
#[derive(Debug, Clone, PartialEq)]
pub struct NewDecision {
    /// The report target the release matched, if any.
    pub target_id: Option<i64>,
    /// The kernel that ran, if one did.
    pub kernel_id: Option<i64>,
    /// MFN's news id of the release; one decision per release.
    pub news_id: String,
    /// The company's ticker.
    pub ticker: String,
    /// The company's exchange.
    pub exchange_mic: String,
    /// The release's language.
    pub lang: String,
    /// The release's fiscal period, when MFN gave one.
    pub fiscal_period: Option<String>,
    /// When MFN published the release.
    pub published_at: DateTime<Utc>,
    /// When the analyst consumed the event.
    pub received_at: DateTime<Utc>,
    /// When the decision was made.
    pub decided_at: DateTime<Utc>,
    /// The decision.
    pub decision: DecisionKind,
    /// Why, in a sentence.
    pub reason: Option<String>,
    /// The kernel's metrics.
    pub metrics: Vec<KernelMetric>,
    /// Microseconds the kernel took.
    pub kernel_micros: Option<i64>,
    /// The error kind of a kernel error.
    pub error: Option<String>,
    /// The trace the decision was made in (32 lowercase hex).
    pub trace_id: Option<String>,
}

/// A stored decision.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct StoredDecision {
    /// The decision's id.
    pub id: i64,
    /// The report target, if any.
    pub target_id: Option<i64>,
    /// The kernel, if one ran.
    pub kernel_id: Option<i64>,
    /// MFN's news id of the release.
    pub news_id: String,
    /// The company's ticker.
    pub ticker: String,
    /// The company's exchange.
    pub exchange_mic: String,
    /// The release's language.
    pub lang: String,
    /// The release's fiscal period.
    pub fiscal_period: Option<String>,
    /// When MFN published the release.
    pub published_at: DateTime<Utc>,
    /// When the analyst consumed the event.
    pub received_at: DateTime<Utc>,
    /// When the decision was made.
    pub decided_at: DateTime<Utc>,
    /// A [`DecisionKind`] as stored.
    pub decision: String,
    /// Why.
    pub reason: Option<String>,
    /// The kernel's metrics.
    pub metrics: Json<Vec<KernelMetric>>,
    /// Microseconds the kernel took.
    pub kernel_micros: Option<i64>,
    /// The error kind of a kernel error.
    pub error: Option<String>,
    /// The trace the decision was made in.
    pub trace_id: Option<String>,
}

impl StoredDecision {
    /// The decision, parsed.
    pub fn kind(&self) -> Result<DecisionKind, String> {
        self.decision.parse()
    }
}

/// What recording a decision did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// The row was written. `kind` is what was stored: a decision for a report
    /// that was already settled is stored as [`DecisionKind::TwinIgnored`].
    Inserted {
        /// The new row's id.
        id: i64,
        /// The decision as stored.
        kind: DecisionKind,
    },
    /// This release already has a decision; nothing changed.
    Duplicate,
}

const DECISION_COLUMNS: &str = "id, target_id, kernel_id, news_id, ticker, exchange_mic, lang, \
     fiscal_period, published_at, received_at, decided_at, decision, reason, metrics, \
     kernel_micros, error, trace_id";

/// Releases of one company this close in time are language versions of the same
/// report, for a report that has no target to tell them apart by.
pub const TWIN_WINDOW_MINUTES: i32 = 10;

impl Store {
    /// Records a decision, once per release.
    ///
    /// In one transaction: when the decision settles a report that has a target,
    /// the target row is locked, and if it was already decided or skipped the
    /// decision is stored as `twin_ignored` instead (so two language versions
    /// racing each other still give one decision per report); the row is written
    /// with `ON CONFLICT (news_id) DO NOTHING`, so a redelivery returns
    /// [`Recorded::Duplicate`] and changes nothing; and for a settling decision the
    /// target becomes `decided` or `skipped`, its running preparation runs are
    /// closed, and the kernel that ran becomes `spent`.
    pub async fn record_decision(&self, new: &NewDecision) -> Result<Recorded, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let mut kind = new.decision;
        let mut reason = new.reason.clone();
        if kind.settles_report() {
            if let Some(target_id) = new.target_id {
                let status: Option<(String,)> =
                    sqlx::query_as("SELECT status FROM report_targets WHERE id = $1 FOR UPDATE")
                        .bind(target_id)
                        .fetch_optional(&mut *tx)
                        .await?;
                if let Some((status,)) = status {
                    if matches!(status.as_str(), "decided" | "skipped") {
                        kind = DecisionKind::TwinIgnored;
                        reason = Some(format!("the report is already {status}"));
                    }
                }
            }
        }
        let inserted: Option<(i64,)> = sqlx::query_as(
            "INSERT INTO decisions (target_id, kernel_id, news_id, ticker, exchange_mic, lang, fiscal_period,
                 published_at, received_at, decided_at, decision, reason, metrics, kernel_micros, error, trace_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
             ON CONFLICT (news_id) DO NOTHING
             RETURNING id",
        )
        .bind(new.target_id)
        .bind(new.kernel_id)
        .bind(&new.news_id)
        .bind(&new.ticker)
        .bind(&new.exchange_mic)
        .bind(&new.lang)
        .bind(&new.fiscal_period)
        .bind(new.published_at)
        .bind(new.received_at)
        .bind(new.decided_at)
        .bind(kind.as_str())
        .bind(&reason)
        .bind(Json(&new.metrics))
        .bind(new.kernel_micros)
        .bind(&new.error)
        .bind(&new.trace_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((id,)) = inserted else {
            return Ok(Recorded::Duplicate);
        };
        if let Some(status) = kind.target_status() {
            if let Some(target_id) = new.target_id {
                sqlx::query(
                    "UPDATE report_targets SET status = $2, updated_at = $3
                     WHERE id = $1 AND status IN ('waiting', 'preparing', 'ready', 'failed')",
                )
                .bind(target_id)
                .bind(status.as_str())
                .bind(new.decided_at)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "UPDATE prep_runs SET outcome = 'failed', finished_at = $2, detail = 'report released'
                     WHERE target_id = $1 AND outcome = 'running'",
                )
                .bind(target_id)
                .bind(new.decided_at)
                .execute(&mut *tx)
                .await?;
            }
            if let Some(kernel_id) = new.kernel_id {
                sqlx::query(
                    "UPDATE kernels SET status = 'spent' WHERE id = $1 AND status = 'accepted'",
                )
                .bind(kernel_id)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(Recorded::Inserted { id, kind })
    }

    /// The decision of an earlier language version of the same report, if any: a
    /// decision that is not itself a twin, for the same target, or for the same
    /// company published within [`TWIN_WINDOW_MINUTES`] of `published_at`.
    pub async fn earlier_decision(
        &self,
        ticker: &str,
        exchange_mic: &str,
        target_id: Option<i64>,
        published_at: DateTime<Utc>,
        news_id: &str,
    ) -> Result<Option<StoredDecision>, sqlx::Error> {
        sqlx::query_as(&format!(
            "SELECT {DECISION_COLUMNS} FROM decisions
             WHERE decision <> 'twin_ignored' AND news_id <> $5
               AND ticker = $1 AND exchange_mic = $2
               AND (($3::bigint IS NOT NULL AND target_id = $3)
                    OR published_at BETWEEN $4 - make_interval(mins => $6) AND $4 + make_interval(mins => $6))
             ORDER BY decided_at, id
             LIMIT 1"
        ))
        .bind(ticker)
        .bind(exchange_mic)
        .bind(target_id)
        .bind(published_at)
        .bind(news_id)
        .bind(TWIN_WINDOW_MINUTES)
        .fetch_optional(&self.pool)
        .await
    }

    /// One decision by the news id of its release.
    pub async fn decision_by_news_id(
        &self,
        news_id: &str,
    ) -> Result<Option<StoredDecision>, sqlx::Error> {
        sqlx::query_as(&format!(
            "SELECT {DECISION_COLUMNS} FROM decisions WHERE news_id = $1"
        ))
        .bind(news_id)
        .fetch_optional(&self.pool)
        .await
    }
}
