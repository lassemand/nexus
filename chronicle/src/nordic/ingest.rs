//! Ingestion of MFN news for a set of companies, followed by an immediate MFN
//! calendar refresh for companies that just announced a calendar change.

use super::calendar::{self, MfnClient, Refresh};
use super::classify::classify_item;
use super::db::{self, Company, NewRelease, Recorded};
use super::feed::{fetch_feed_page, first_page_url};
use chrono::Duration;
use sqlx::PgPool;
use tracing::{error, info, warn};
use uuid::Uuid;

/// Safety cap on pages fetched per company and run.
pub const MAX_PAGES: usize = 100;

/// Incremental runs stop once a page has nothing new and its oldest item is
/// more than this many days older than the newest stored release.
const STOP_AFTER_DAYS: i64 = 14;

/// Result of ingesting one company.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CompanyOutcome {
    /// Rows newly inserted or filled into a scheduled row.
    pub inserted: usize,
    /// Newly inserted financial-calendar announcements.
    pub new_calendar_updates: usize,
    /// `mfn_news_id`s of those announcements.
    pub new_calendar_ids: Vec<Uuid>,
    /// Pages fetched.
    pub pages: usize,
}

/// Summary of a [`process_companies`] run.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RunSummary {
    /// Companies with an `mfn_slug` that were attempted.
    pub attempted: usize,
    /// Of those, how many failed.
    pub failed: usize,
    /// Tickers with newly inserted calendar announcements.
    pub calendar_updated: Vec<String>,
    /// Tickers whose MFN calendar was refreshed after an announcement.
    pub calendars_refreshed: Vec<String>,
}

impl RunSummary {
    /// The run counts as failed if any attempted company failed.
    pub fn is_success(&self) -> bool {
        self.failed == 0
    }
}

/// Ingest the MFN news feed for one company.
///
/// Pages newest first following `next_url`. Without `backfill`, paging stops at
/// the first page that has no new item and whose oldest item is more than 14
/// days older than the newest stored release; with it, paging runs to the end.
pub async fn ingest_company(
    pool: &PgPool,
    feed: &mut MfnClient,
    company: &Company,
    slug: &str,
    backfill: bool,
) -> anyhow::Result<CompanyOutcome> {
    let mut outcome = CompanyOutcome::default();
    let newest = db::newest_occurred_at(pool, &company.ticker, &company.exchange_mic).await?;
    let mic_prefix = format!("{}:", company.exchange_mic);
    let mut checked_tickers = false;
    let mut url = first_page_url(&feed.base()?, slug)?;

    loop {
        let page = fetch_feed_page(feed, url).await?;
        outcome.pages += 1;
        if page.skipped > 0 {
            warn!(ticker = %company.ticker, skipped = page.skipped, "feed items with bad publish_date skipped");
        }
        let oldest = page.items.iter().map(|i| i.published_at).min();

        let mut new_in_page = 0;
        for item in &page.items {
            if !item.is_authored_by(slug) {
                continue;
            }
            if !checked_tickers {
                checked_tickers = true;
                if !item
                    .author_tickers
                    .iter()
                    .any(|t| t.starts_with(&mic_prefix))
                {
                    warn!(
                        ticker = %company.ticker, slug, tickers = ?item.author_tickers,
                        "mapping may point at the wrong company"
                    );
                }
            }
            let c = classify_item(&item.tags, &item.title);
            let row = NewRelease {
                ticker: &company.ticker,
                exchange_mic: &company.exchange_mic,
                mfn_news_id: item.news_id,
                occurred_at: item.published_at,
                report_kind: c.report_kind.map(|k| k.as_str()),
                headline: &item.title,
                url: item.url.as_deref(),
                lang: item.lang.as_deref(),
                tags: &item.tags,
                fiscal_period: c.fiscal_period.as_deref(),
                is_correction: c.is_correction,
                calendar_update: c.calendar_update,
            };
            let recorded = db::record_release(pool, &row).await?;
            if recorded.is_new() {
                new_in_page += 1;
                outcome.inserted += 1;
            }
            if recorded == Recorded::Inserted && c.calendar_update {
                outcome.new_calendar_updates += 1;
                outcome.new_calendar_ids.push(item.news_id);
            }
        }

        let Some(next) = page.next_url else { break };
        let caught_up = newest
            .zip(oldest)
            .is_some_and(|(n, o)| n - o > Duration::days(STOP_AFTER_DAYS));
        if !backfill && new_in_page == 0 && caught_up {
            break;
        }
        if outcome.pages >= MAX_PAGES {
            error!(ticker = %company.ticker, pages = outcome.pages, "page cap reached, stopping");
            break;
        }
        url = next;
    }
    if outcome.inserted > 0 {
        db::flag_resends(pool, &company.ticker, &company.exchange_mic).await?;
        db::link_corrections(pool, &company.ticker, &company.exchange_mic).await?;
    }
    Ok(outcome)
}

/// Ingest every mapped company, then refresh the MFN calendar once for each
/// company that got a newly inserted calendar announcement. A failure for one
/// company (or one refresh) is logged and does not stop the others.
pub async fn process_companies(
    pool: &PgPool,
    feed: &mut MfnClient,
    calendar_client: &mut MfnClient,
    companies: &[Company],
    backfill: bool,
) -> RunSummary {
    let mut summary = RunSummary::default();
    let mut to_refresh: Vec<(&Company, Uuid)> = Vec::new();
    for c in companies {
        let Some(slug) = c.mfn_slug.as_deref() else {
            info!(ticker = %c.ticker, mic = %c.exchange_mic, "no mfn_slug mapping, skipping");
            continue;
        };
        summary.attempted += 1;
        match ingest_company(pool, feed, c, slug, backfill).await {
            Ok(o) => {
                info!(ticker = %c.ticker, inserted = o.inserted, pages = o.pages, "ingested");
                if let Some(first) = o.new_calendar_ids.first() {
                    summary.calendar_updated.push(c.ticker.clone());
                    to_refresh.push((c, *first));
                }
            }
            Err(e) => {
                summary.failed += 1;
                error!(ticker = %c.ticker, error = %e, "company failed");
            }
        }
    }
    if !summary.calendar_updated.is_empty() {
        info!(tickers = ?summary.calendar_updated, "calendar_updated: new financial-calendar announcements");
    }
    for (c, news_id) in to_refresh {
        match calendar::refresh_company(pool, calendar_client, &c.ticker, &c.exchange_mic).await {
            Ok(Refresh::Refreshed(_)) => {
                info!(ticker = %c.ticker, %news_id, "calendar refreshed after announcement");
                summary.calendars_refreshed.push(c.ticker.clone());
            }
            Ok(Refresh::NoSlug) => {
                info!(ticker = %c.ticker, %news_id, "calendar announcement but no mfn_slug, skipping refresh");
            }
            // The daily nordic-calendar job retries.
            Err(e) => warn!(ticker = %c.ticker, %news_id, error = %e, "calendar refresh failed"),
        }
    }
    summary
}
