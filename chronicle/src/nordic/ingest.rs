//! Publishing each company's MFN news as special events, followed by an
//! immediate calendar refresh for companies that just announced a change.
//!
//! Progress is kept per company in `mfn_ingest_state.feed_cursor`: the
//! `publish_date` of the newest item published. A run pages back to one day
//! before it (MFN sometimes lists items late; re-sending them is harmless,
//! signal upserts by `(source, source_id)`), and moves it only after every
//! message for the company was acknowledged, so a failed publish loses nothing.

use super::calendar::{self, MfnClient, Refresh};
use super::classify::classify_item;
use super::db::{self, Company};
use super::events::{publish, release_event, EventSink};
use super::feed::{fetch_feed_page, first_page_url};
use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use tracing::{error, info, warn};

/// Safety cap on pages fetched per company and run.
pub const MAX_PAGES: usize = 100;

/// How far before the cursor a run keeps reading.
const CURSOR_OVERLAP: Duration = Duration::days(1);

/// Result of ingesting one company.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CompanyOutcome {
    /// Events published (reports and corrections).
    pub published: usize,
    /// A calendar notice newer than the cursor at the start of the run.
    pub calendar_announced: bool,
    /// Pages fetched.
    pub pages: usize,
    /// The cursor after the run.
    pub cursor: Option<DateTime<Utc>>,
}

/// Summary of a [`process_companies`] run.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RunSummary {
    /// Companies with an `mfn_slug` that were attempted.
    pub attempted: usize,
    /// Of those, how many failed.
    pub failed: usize,
    /// Tickers with a new calendar announcement.
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

/// Publish one company's MFN news feed.
///
/// Pages newest first following `next_url`, until a page reaches items older
/// than `feed_cursor - 1 day`. Without a cursor (a new company), or with
/// `backfill`, it pages to the end.
pub async fn ingest_company(
    pool: &PgPool,
    feed: &mut MfnClient,
    sink: &dyn EventSink,
    company: &Company,
    slug: &str,
    backfill: bool,
) -> anyhow::Result<CompanyOutcome> {
    let start = db::load_state(pool, &company.ticker, &company.exchange_mic)
        .await?
        .feed_cursor;
    let stop_before = if backfill {
        None
    } else {
        start.map(|c| c - CURSOR_OVERLAP)
    };
    let mut outcome = CompanyOutcome {
        cursor: start,
        ..Default::default()
    };
    let mut newest_published: Option<DateTime<Utc>> = None;
    let mic_prefix = format!("{}:", company.exchange_mic);
    let mut checked_tickers = false;
    let mut checked_isin = false;
    let mut url = first_page_url(&feed.base()?, slug)?;

    loop {
        let page = fetch_feed_page(feed, url).await?;
        outcome.pages += 1;
        if page.skipped > 0 {
            warn!(ticker = %company.ticker, skipped = page.skipped, "feed items with bad publish_date skipped");
        }
        let oldest = page.items.iter().map(|i| i.published_at).min();

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
            if !checked_isin {
                checked_isin = true;
                store_isin(pool, company, slug, &item.author_isins).await;
            }
            let c = classify_item(&item.tags, &item.title);
            if c.calendar_update && start.is_none_or(|s| item.published_at > s) {
                outcome.calendar_announced = true;
            }
            if let Some(event) = release_event(company, item, &c) {
                publish(sink, company, &event).await?;
                outcome.published += 1;
                newest_published = newest_published.max(Some(item.published_at));
            }
        }

        let Some(next) = page.next_url else { break };
        if stop_before.zip(oldest).is_some_and(|(stop, o)| o < stop) {
            break;
        }
        if outcome.pages >= MAX_PAGES {
            error!(ticker = %company.ticker, pages = outcome.pages, "page cap reached, stopping");
            break;
        }
        url = next;
    }

    // Every message for the company was acknowledged: the cursor may move.
    if let Some(newest) = newest_published {
        db::advance_feed_cursor(pool, &company.ticker, &company.exchange_mic, newest).await?;
        outcome.cursor = outcome.cursor.max(Some(newest));
    }
    Ok(outcome)
}

/// The one valid ISIN in MFN's `author.isins`, if there is exactly one.
/// Several (or none valid) is ambiguous: an ISIN is never guessed.
fn unique_valid_isin(isins: &[String]) -> Option<&str> {
    let mut valid = isins
        .iter()
        .map(String::as_str)
        .filter(|i| model::isin::is_valid(i));
    let first = valid.next()?;
    valid.next().is_none().then_some(first)
}

/// Fills `companies.isin` from the feed's author data when it is still empty.
/// Best effort: a problem is logged and never stops the ingest.
async fn store_isin(pool: &PgPool, company: &Company, slug: &str, isins: &[String]) {
    let Some(isin) = unique_valid_isin(isins) else {
        if !isins.is_empty() {
            warn!(ticker = %company.ticker, slug, isins = ?isins,
                  "MFN lists no single valid ISIN, leaving companies.isin unset");
        }
        return;
    };
    match db::fill_isin(pool, &company.ticker, &company.exchange_mic, isin).await {
        Ok(true) => info!(ticker = %company.ticker, isin, "stored ISIN from MFN"),
        Ok(false) => {}
        Err(e) => warn!(ticker = %company.ticker, isin, error = %e, "could not store ISIN"),
    }
}

/// Publish every mapped company, then refresh the MFN calendar once for each
/// company that announced a calendar change. A failure for one company (or
/// one refresh) is logged and does not stop the others.
pub async fn process_companies(
    pool: &PgPool,
    feed: &mut MfnClient,
    calendar_client: &mut MfnClient,
    sink: &dyn EventSink,
    companies: &[Company],
    backfill: bool,
) -> RunSummary {
    let mut summary = RunSummary::default();
    let mut to_refresh: Vec<&Company> = Vec::new();
    for c in companies {
        let Some(slug) = c.mfn_slug.as_deref() else {
            info!(ticker = %c.ticker, mic = %c.exchange_mic, "no mfn_slug mapping, skipping");
            continue;
        };
        summary.attempted += 1;
        match ingest_company(pool, feed, sink, c, slug, backfill).await {
            Ok(o) => {
                info!(ticker = %c.ticker, published = o.published, pages = o.pages,
                      cursor = ?o.cursor, "ingested");
                if o.calendar_announced {
                    summary.calendar_updated.push(c.ticker.clone());
                    to_refresh.push(c);
                }
            }
            Err(e) => {
                summary.failed += 1;
                error!(ticker = %c.ticker, error = %e, "company failed, cursor not moved");
            }
        }
    }
    if !summary.calendar_updated.is_empty() {
        info!(tickers = ?summary.calendar_updated, "calendar_updated: new financial-calendar announcements");
    }
    for c in to_refresh {
        match calendar::refresh_company(pool, calendar_client, sink, c, false).await {
            Ok(Refresh::NoSlug) => {}
            Ok(r) => {
                info!(ticker = %c.ticker, outcome = ?r, "calendar refreshed after announcement");
                summary.calendars_refreshed.push(c.ticker.clone());
            }
            // The daily nordic-calendar job retries.
            Err(e) => warn!(ticker = %c.ticker, error = %e, "calendar refresh failed"),
        }
    }
    summary
}

#[cfg(test)]
mod isin_tests {
    use super::unique_valid_isin;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn isin_is_taken_only_when_unambiguous() {
        assert_eq!(
            unique_valid_isin(&v(&["DK0062498333"])),
            Some("DK0062498333")
        );
        assert_eq!(unique_valid_isin(&v(&["DK0062498332"])), None);
        assert_eq!(unique_valid_isin(&v(&[])), None);
        assert_eq!(
            unique_valid_isin(&v(&["DK0062498333", "SE0008348304"])),
            None
        );
        assert_eq!(
            unique_valid_isin(&v(&["bad", "SE0008348304"])),
            Some("SE0008348304")
        );
    }
}
