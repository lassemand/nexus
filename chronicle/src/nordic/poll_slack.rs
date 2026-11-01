//! Slack fallback for the `nordic-reports` poll (NEX-223): posts results
//! releases that `nordic-live` did not post, marked late. The claim in
//! `slack_report_posts` keeps every report to one message.

use super::classify::strip_issuer_prefix;
use super::classify::Classification;
use super::db::Company;
use super::feed::NewsItem;
use super::slack::{
    deliver_notice, display_name, notice_time_zone, Outcome, Poster, ReportNotice, SlackApi,
};
use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;

/// Items published longer ago than this are not posted. The poll runs once a
/// day (22:00 Europe/Stockholm), so 48 hours cover one missed or failed run plus
/// the day itself; anything older (a long outage, a new company's first sync, a
/// cursor reset) would only be a flood of stale posts.
pub const MAX_AGE: Duration = Duration::hours(48);

/// What the Slack fallback did in one run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SlackStats {
    /// New posts (late).
    pub posted: usize,
    /// Non-English posts replaced by the English twin.
    pub updated: usize,
    /// Slack or claim failures.
    pub failed: usize,
    /// Results releases older than [`MAX_AGE`], not posted.
    pub skipped_old: usize,
}

impl SlackStats {
    /// Adds another count.
    pub fn add(&mut self, other: &SlackStats) {
        self.posted += other.posted;
        self.updated += other.updated;
        self.failed += other.failed;
        self.skipped_old += other.skipped_old;
    }
}

/// The Slack side of a poll run.
pub struct PollSlack {
    pool: PgPool,
    api: Box<dyn SlackApi>,
    now: DateTime<Utc>,
}

impl PollSlack {
    /// A fallback poster; the age limit counts back from now.
    pub fn new(pool: PgPool, api: Box<dyn SlackApi>) -> Self {
        Self::at(pool, api, Utc::now())
    }

    /// Like [`PollSlack::new`] with a fixed clock (tests).
    pub fn at(pool: PgPool, api: Box<dyn SlackApi>, now: DateTime<Utc>) -> Self {
        Self { pool, api, now }
    }

    /// Posts a results release the poll just published, unless `nordic-live`
    /// already did. Never fails: problems are counted in `stats` and logged.
    pub async fn handle(
        &self,
        company: &Company,
        item: &NewsItem,
        class: &Classification,
        stats: &mut SlackStats,
    ) {
        let Some(report_kind) = class.report_kind.filter(|_| !class.is_correction) else {
            return;
        };
        if item.published_at < self.now - MAX_AGE {
            stats.skipped_old += 1;
            return;
        }
        let notice = ReportNotice {
            company_name: display_name(
                company.name.as_deref(),
                item.author_name.as_deref(),
                &company.ticker,
            ),
            ticker: company.ticker.clone(),
            exchange_mic: company.exchange_mic.clone(),
            report_kind,
            fiscal_period: class.fiscal_period.clone(),
            published_at: item.published_at,
            time_zone: notice_time_zone(&company.exchange_mic),
            session: None,
            headline: strip_issuer_prefix(&item.title).to_string(),
            mfn_url: item.url.clone(),
            pdf_url: item.attachment_urls.first().cloned(),
            late: true,
            news_id: item.news_id,
            lang: item.lang.clone(),
        };
        match deliver_notice(&self.pool, self.api.as_ref(), &notice, Poster::Poll).await {
            Outcome::Posted => stats.posted += 1,
            Outcome::Updated => stats.updated += 1,
            Outcome::Failed => stats.failed += 1,
            Outcome::SkippedAlreadyPosted | Outcome::InFlight => {}
        }
    }
}
