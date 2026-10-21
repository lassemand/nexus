//! Ingestion of Nasdaq Nordic news for a set of companies, followed by an
//! immediate MFN calendar refresh for companies that just announced a change.

use super::calendar::{self, MfnClient, Refresh};
use super::classify;
use super::db::{self, Company, NewRelease, Recorded};
use super::feed::{NewsClient, PAGE_SIZE};
use sqlx::PgPool;
use tracing::{error, info, warn};

/// Result of ingesting one company.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CompanyOutcome {
    /// Rows newly inserted or filled into a scheduled row.
    pub inserted: usize,
    /// `disclosure_id`s of newly inserted financial-calendar announcements.
    pub new_calendar_disclosures: Vec<i64>,
}

/// Summary of a [`process_companies`] run.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RunSummary {
    /// Companies with a Nasdaq mapping that were attempted.
    pub attempted: usize,
    /// Of those, how many failed.
    pub failed: usize,
    /// Tickers whose MFN calendar was refreshed after an announcement.
    pub calendars_refreshed: Vec<String>,
}

impl RunSummary {
    /// The run counts as failed only if every attempted company failed.
    pub fn is_success(&self) -> bool {
        !(self.attempted > 0 && self.failed == self.attempted)
    }
}

/// Ingest all pages for one company until a known item is reached.
pub async fn ingest_company(
    pool: &PgPool,
    client: &mut NewsClient,
    company: &Company,
    issuer: &str,
) -> anyhow::Result<CompanyOutcome> {
    let mut outcome = CompanyOutcome::default();
    let mut start = 0;
    loop {
        let page = client.fetch_page(issuer, start).await?;
        if page.items.is_empty() {
            break;
        }
        let mut hit_known = false;
        for item in &page.items {
            if item.company != issuer {
                warn!(ticker = %company.ticker, got = %item.company, "skipping item from another issuer");
                continue;
            }
            let released_at = match item.released_at() {
                Ok(t) => t,
                Err(e) => {
                    warn!(disclosure_id = item.disclosure_id, error = %e, "skipping item");
                    continue;
                }
            };
            let is_correction = classify::is_correction(&item.headline);
            let calendar_update = classify::is_calendar_update(&item.headline);
            let report_kind =
                classify::report_kind(&item.cns_category, &item.headline).map(|k| k.as_str());
            let row = NewRelease {
                ticker: &company.ticker,
                exchange_mic: &company.exchange_mic,
                disclosure_id: item.disclosure_id,
                released_at,
                category: &item.cns_category,
                report_kind,
                headline: &item.headline,
                url: item.message_url.as_deref(),
                is_correction,
                calendar_update,
            };
            match db::record_release(pool, &row).await? {
                Recorded::Updated => hit_known = true,
                Recorded::Matched(_) => outcome.inserted += 1,
                Recorded::Inserted => {
                    outcome.inserted += 1;
                    if calendar_update {
                        outcome.new_calendar_disclosures.push(item.disclosure_id);
                    }
                }
            }
        }
        start += page.items.len();
        if hit_known || start >= page.total || page.items.len() < PAGE_SIZE {
            break;
        }
    }
    if outcome.inserted > 0 {
        db::link_corrections(pool, &company.ticker, &company.exchange_mic).await?;
    }
    Ok(outcome)
}

/// Ingest every mapped company, then refresh the MFN calendar once for each
/// company that got a newly inserted calendar announcement. A failure for one
/// company (or one refresh) is logged and does not stop the others.
pub async fn process_companies(
    pool: &PgPool,
    news: &mut NewsClient,
    mfn: &mut MfnClient,
    companies: &[Company],
) -> RunSummary {
    let mut summary = RunSummary::default();
    let mut to_refresh: Vec<(&Company, i64)> = Vec::new();
    for c in companies {
        let Some(issuer) = c.nasdaq_news_company.as_deref() else {
            info!(ticker = %c.ticker, mic = %c.exchange_mic, "no nasdaq_news_company mapping, skipping");
            continue;
        };
        summary.attempted += 1;
        match ingest_company(pool, news, c, issuer).await {
            Ok(o) => {
                info!(ticker = %c.ticker, inserted = o.inserted, "ingested");
                if let Some(first) = o.new_calendar_disclosures.first() {
                    to_refresh.push((c, *first));
                }
            }
            Err(e) => {
                summary.failed += 1;
                error!(ticker = %c.ticker, error = %e, "company failed");
            }
        }
    }
    for (c, disclosure_id) in to_refresh {
        match calendar::refresh_company(pool, mfn, &c.ticker, &c.exchange_mic).await {
            Ok(Refresh::Refreshed(_)) => {
                info!(ticker = %c.ticker, disclosure_id, "calendar refreshed after announcement");
                summary.calendars_refreshed.push(c.ticker.clone());
            }
            Ok(Refresh::NoSlug) => {
                info!(ticker = %c.ticker, disclosure_id, "calendar announcement but no mfn_slug, skipping refresh");
            }
            // The daily nordic-calendar job retries.
            Err(e) => {
                warn!(ticker = %c.ticker, disclosure_id, error = %e, "calendar refresh failed")
            }
        }
    }
    summary
}
