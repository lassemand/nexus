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
use super::feed::{fetch_feed_page, first_page_url, NewsItem};
use super::insiders::{self, insider_key, InsiderSink, InsiderStats};
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
    /// Insider transactions published (Finnish companies).
    pub insiders_published: usize,
    /// What the managers' transaction releases came to.
    pub insiders: InsiderStats,
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
    /// Insider transactions published.
    pub insiders_published: usize,
    /// Managers' transaction releases that did not parse.
    pub mar_parse_errors: usize,
    /// Releases rejected for a zero price on a BUY or SELL.
    pub mar_suspicious_zero_price: usize,
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
    insider_sink: &dyn InsiderSink,
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
    let mut checked_tickers = false;
    let mut checked_isin = false;
    let mut url = first_page_url(&feed.base()?, slug)?;
    // Managers' transactions of a Finnish company, parsed after the paging so a
    // release and its twin are seen together.
    let finnish = insiders::uses_mar_releases(&company.exchange_mic);
    let mut insider_items: Vec<NewsItem> = Vec::new();

    loop {
        let page = fetch_feed_page(feed, url).await?;
        outcome.pages += 1;
        if page.skipped > 0 {
            warn!(ticker = %company.ticker, skipped = page.skipped, "feed items with bad publish_date skipped");
        }
        let oldest = page.items.iter().map(|i| i.published_at).min();

        for item in &page.items {
            if !belongs_to(company, slug, item) {
                continue;
            }
            if !checked_tickers {
                checked_tickers = true;
                refuse_wrong_company(company, slug, item)?;
            }
            if !checked_isin {
                checked_isin = true;
                store_isin(pool, company, slug, &item.author_isins).await;
            }
            if finnish && insiders::is_managers_transaction(item) {
                insider_items.push(item.clone());
                continue;
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

    // Insider transactions go out before the cursor moves: a failed send leaves
    // it where it was, and the next run re-sends (the ids are stable).
    if !insider_items.is_empty() {
        let refs: Vec<&NewsItem> = insider_items.iter().collect();
        let batch = insiders::build(company, &refs);
        for (published_at, msg) in &batch.messages {
            insider_sink.send(&insider_key(msg), msg).await?;
            outcome.insiders_published += 1;
            newest_published = newest_published.max(Some(*published_at));
        }
        count_insider_metrics(company, &batch.stats, outcome.insiders_published);
        outcome.insiders = batch.stats;
        // A release that does not parse today will not parse tomorrow: it must
        // not hold the cursor back, so every managers' transaction read counts.
        newest_published = newest_published.max(insider_items.iter().map(|i| i.published_at).max());
    }

    // Every message for the company was acknowledged: the cursor may move.
    if let Some(newest) = newest_published {
        db::advance_feed_cursor(pool, &company.ticker, &company.exchange_mic, newest).await?;
        outcome.cursor = outcome.cursor.max(Some(newest));
    }
    Ok(outcome)
}

/// Fails if the feed's first item is not this company's: two MFN companies can
/// share a name, and publishing the other one's releases as this company's is
/// worse than publishing nothing.
fn refuse_wrong_company(company: &Company, slug: &str, item: &NewsItem) -> anyhow::Result<()> {
    let problems = mapping_problems(company, &item.author_tickers, &item.author_isins);
    if problems.is_empty() {
        return Ok(());
    }
    for problem in &problems {
        error!(ticker = %company.ticker, slug, problem = %problem,
               "mapping points at the wrong company, nothing published");
    }
    anyhow::bail!(
        "mfn_slug '{slug}' is not {}:{}: {}",
        company.exchange_mic,
        company.ticker,
        problems.join("; ")
    )
}

/// Counters for the managers' transaction releases of one company.
fn count_insider_metrics(company: &Company, stats: &InsiderStats, published: usize) {
    let mic = company.exchange_mic.clone();
    metrics::counter!("nordic_mar_parse_errors_total", "exchange_mic" => mic.clone())
        .increment(stats.parse_errors as u64);
    metrics::counter!("nordic_mar_suspicious_zero_price_total", "exchange_mic" => mic.clone())
        .increment(stats.suspicious_zero_price as u64);
    metrics::counter!("nordic_mar_transactions_published_total", "exchange_mic" => mic)
        .increment(published as u64);
    if stats.before_mar > 0 {
        info!(ticker = %company.ticker, before_mar = stats.before_mar,
              "managers' transactions before MAR (2016-07-03) ignored");
    }
}

/// Pages one company's feed back to `from` and returns its managers'
/// transaction releases with the number of pages read. Publishes nothing.
pub async fn collect_insider_items(
    feed: &mut MfnClient,
    company: &Company,
    slug: &str,
    from: DateTime<Utc>,
) -> anyhow::Result<(Vec<NewsItem>, usize)> {
    anyhow::ensure!(
        insiders::uses_mar_releases(&company.exchange_mic),
        "{}:{} does not take its insiders from MFN releases",
        company.exchange_mic,
        company.ticker
    );
    let mut pages = 0;
    let mut items: Vec<NewsItem> = Vec::new();
    let mut checked = false;
    let mut url = first_page_url(&feed.base()?, slug)?;
    loop {
        let page = fetch_feed_page(feed, url).await?;
        pages += 1;
        let oldest = page.items.iter().map(|i| i.published_at).min();
        for item in &page.items {
            if !belongs_to(company, slug, item) {
                continue;
            }
            if !checked {
                checked = true;
                refuse_wrong_company(company, slug, item)?;
            }
            if item.published_at >= from && insiders::is_managers_transaction(item) {
                items.push(item.clone());
            }
        }
        let Some(next) = page.next_url else { break };
        if oldest.is_some_and(|o| o < from) {
            break;
        }
        if pages >= MAX_PAGES {
            error!(ticker = %company.ticker, pages, "page cap reached, stopping");
            break;
        }
        url = next;
    }
    Ok((items, pages))
}

/// Re-reads one company's feed for managers' transactions only (the history,
/// or a re-send). Publishes no report events and touches no cursor, so it can
/// run at any time and as often as needed: the transaction ids are stable and
/// signal's upsert is idempotent. Pages back to `from`.
pub async fn ingest_insiders_only(
    feed: &mut MfnClient,
    insider_sink: &dyn InsiderSink,
    company: &Company,
    slug: &str,
    from: DateTime<Utc>,
) -> anyhow::Result<CompanyOutcome> {
    let mut outcome = CompanyOutcome::default();
    let (items, pages) = collect_insider_items(feed, company, slug, from).await?;
    outcome.pages = pages;
    let refs: Vec<&NewsItem> = items.iter().collect();
    let batch = insiders::build(company, &refs);
    for (_, msg) in &batch.messages {
        insider_sink.send(&insider_key(msg), msg).await?;
        outcome.insiders_published += 1;
    }
    count_insider_metrics(company, &batch.stats, outcome.insiders_published);
    outcome.insiders = batch.stats;
    Ok(outcome)
}

/// [`ingest_insiders_only`] for every Finnish company with an `mfn_slug`.
pub async fn process_insiders_only(
    feed: &mut MfnClient,
    insider_sink: &dyn InsiderSink,
    companies: &[Company],
    from: DateTime<Utc>,
) -> RunSummary {
    let mut summary = RunSummary::default();
    for c in companies {
        let Some(slug) = c.mfn_slug.as_deref() else {
            continue;
        };
        if !insiders::uses_mar_releases(&c.exchange_mic) {
            continue;
        }
        summary.attempted += 1;
        match ingest_insiders_only(feed, insider_sink, c, slug, from).await {
            Ok(o) => {
                info!(ticker = %c.ticker, pages = o.pages, insiders_published = o.insiders_published,
                      insiders = ?o.insiders, "insiders re-read");
                summary.insiders_published += o.insiders_published;
                summary.mar_parse_errors += o.insiders.parse_errors;
                summary.mar_suspicious_zero_price += o.insiders.suspicious_zero_price;
            }
            Err(e) => {
                summary.failed += 1;
                error!(ticker = %c.ticker, error = %e, "insider re-read failed");
            }
        }
    }
    summary
}

/// A ticker as compared across sources: uppercase, spaces and hyphens removed
/// (MFN `NOVO B`, registry `NOVO-B`).
pub fn normalise_ticker(t: &str) -> String {
    t.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .flat_map(char::to_uppercase)
        .collect()
}

/// True if the feed item is this company's: authored under its MFN slug, or
/// (when the registry has the company's ISIN) by an entity with the same ISIN.
///
/// A renamed company keeps its ISIN but not its slug: Metso's feed carries its
/// earlier releases under `outotec` and `metso-outotec`, which only the ISIN ties
/// to `metso-2`. A different company that shares the name has another ISIN.
pub fn belongs_to(company: &Company, slug: &str, item: &NewsItem) -> bool {
    item.is_authored_by(slug)
        || company.isin.as_deref().is_some_and(|isin| {
            item.author_isins
                .iter()
                .any(|i| i.eq_ignore_ascii_case(isin))
        })
}

/// True if `author_tickers` (`MIC:SYMBOL`) lists `ticker` on `exchange_mic`,
/// compared uppercase with spaces and hyphens removed.
pub fn ticker_listed(exchange_mic: &str, ticker: &str, author_tickers: &[String]) -> bool {
    let wanted = normalise_ticker(ticker);
    author_tickers.iter().any(|t| {
        t.split_once(':').is_some_and(|(m, sym)| {
            m.eq_ignore_ascii_case(exchange_mic) && normalise_ticker(sym) == wanted
        })
    })
}

/// What looks wrong about the company's MFN mapping, from the feed's
/// `author.tickers` (`MIC:SYMBOL`) and `author.isins`. Empty if it looks right.
///
/// - No ticker on the company's MIC with the same normalised symbol.
/// - The company has an ISIN that MFN does not list (both values reported).
pub fn mapping_problems(
    company: &Company,
    author_tickers: &[String],
    author_isins: &[String],
) -> Vec<String> {
    let mut out = Vec::new();
    if !ticker_listed(&company.exchange_mic, &company.ticker, author_tickers) {
        out.push(format!(
            "registry {}:{} is not among MFN's tickers {author_tickers:?}",
            company.exchange_mic, company.ticker
        ));
    }
    if let Some(isin) = &company.isin {
        if !author_isins.iter().any(|i| i.eq_ignore_ascii_case(isin)) {
            out.push(format!(
                "registry ISIN {isin} is not among MFN's ISINs {author_isins:?}"
            ));
        }
    }
    out
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
    insider_sink: &dyn InsiderSink,
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
        match ingest_company(pool, feed, sink, insider_sink, c, slug, backfill).await {
            Ok(o) => {
                info!(ticker = %c.ticker, published = o.published, pages = o.pages,
                      cursor = ?o.cursor, insiders_published = o.insiders_published,
                      insiders = ?o.insiders, "ingested");
                summary.insiders_published += o.insiders_published;
                summary.mar_parse_errors += o.insiders.parse_errors;
                summary.mar_suspicious_zero_price += o.insiders.suspicious_zero_price;
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

#[cfg(test)]
mod mapping_tests {
    use super::*;

    fn company(ticker: &str, mic: &str, isin: Option<&str>) -> Company {
        Company {
            ticker: ticker.into(),
            exchange_mic: mic.into(),
            mfn_slug: Some("x".into()),
            name: None,
            isin: isin.map(String::from),
        }
    }

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn tickers_are_compared_without_spaces_hyphens_and_case() {
        assert_eq!(normalise_ticker("novo-b"), "NOVOB");
        assert_eq!(normalise_ticker("NOVO B"), "NOVOB");
        assert_eq!(normalise_ticker("VOLV B"), normalise_ticker("VOLV-B"));
    }

    #[test]
    fn novo_matches_with_the_space_and_the_isin() {
        let c = company("NOVO-B", "XCSE", Some("DK0062498333"));
        let t = v(&["XCSE:NOVO B", "XLON:0QIU"]);
        assert!(mapping_problems(&c, &t, &v(&["DK0062498333"])).is_empty());
        // No ISIN registered: only the ticker is checked.
        let c = company("NOVO-B", "XCSE", None);
        assert!(mapping_problems(&c, &t, &[]).is_empty());
        // GomSpace as before.
        let g = company("GOMX", "FNSE", None);
        assert!(mapping_problems(&g, &v(&["FNSE:GOMX", "XLON:0GE8"]), &[]).is_empty());
    }

    #[test]
    fn a_wrong_ticker_or_exchange_is_reported() {
        let c = company("NOVO-B", "XCSE", None);
        // Same symbol on another MIC does not count.
        assert_eq!(mapping_problems(&c, &v(&["XLON:NOVO B"]), &[]).len(), 1);
        assert_eq!(mapping_problems(&c, &v(&["XCSE:DSV"]), &[]).len(), 1);
        assert_eq!(mapping_problems(&c, &[], &[]).len(), 1);
    }

    // The two MFN entities behind the name "Metso" and today's company.
    const METSO: (&[&str], &[&str]) = (
        &["XHEL:METSO", "CAPA:METSOh", "XLON:0MGI"],
        &["FI0009014575"],
    );
    const NELES: (&[&str], &[&str]) = (&["XHEL:NELES"], &["FI4000440664"]);
    // The old Metso Oyj (before Neles): same ticker symbol, other ISIN.
    const OLD_METSO: (&[&str], &[&str]) = (&["XHEL:METSO"], &["FI0009007835"]);

    #[test]
    fn metso_matches_its_own_mfn_entity() {
        let c = company("METSO", "XHEL", Some("FI0009014575"));
        assert!(mapping_problems(&c, &v(METSO.0), &v(METSO.1)).is_empty());
        // Finnish share classes have no separator; a ticker with one still matches.
        let k = company("KNEBV", "XHEL", None);
        assert!(mapping_problems(&k, &v(&["XHEL:KNEBV"]), &[]).is_empty());
    }

    #[test]
    fn the_neles_and_old_metso_entities_are_rejected_for_metso() {
        let c = company("METSO", "XHEL", Some("FI0009014575"));
        // Neles: wrong ticker and wrong ISIN.
        let p = mapping_problems(&c, &v(NELES.0), &v(NELES.1));
        assert_eq!(p.len(), 2, "{p:?}");
        // Old Metso: the ticker symbol matches, the ISIN does not.
        let p = mapping_problems(&c, &v(OLD_METSO.0), &v(OLD_METSO.1));
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].contains("FI0009014575") && p[0].contains("FI0009007835"));
        // Without the registry ISIN only the ticker can tell Neles apart.
        let no_isin = company("METSO", "XHEL", None);
        assert_eq!(
            mapping_problems(&no_isin, &v(NELES.0), &v(NELES.1)).len(),
            1
        );
    }

    fn news(slug: &str, slugs: &[&str], entity: (&[&str], &[&str])) -> NewsItem {
        NewsItem {
            news_id: uuid::Uuid::nil(),
            url: None,
            lang: Some("en".into()),
            tags: vec![],
            title: "t".into(),
            published_at: Utc::now(),
            author_slug: slug.into(),
            author_slugs: v(slugs),
            author_tickers: v(entity.0),
            author_isins: v(entity.1),
            source: None,
            attachment_urls: vec![],
            text: None,
        }
    }

    #[test]
    fn a_renamed_company_belongs_by_isin_and_a_namesake_does_not() {
        let metso = company("METSO", "XHEL", Some("FI0009014575"));
        // Today's slug, and the Outotec / Metso Outotec entities of the same ISIN.
        let now = news("metso-2", &["metso-2", "metso-outotec", "outotec"], METSO);
        let outotec = news(
            "outotec",
            &["outotec"],
            (&["XHEL:OTE1V"], &["FI0009014575"]),
        );
        assert!(belongs_to(&metso, "metso-2", &now));
        assert!(belongs_to(&metso, "metso-2", &outotec));
        // Without the ISIN in the registry only the slug counts.
        let no_isin = company("METSO", "XHEL", None);
        assert!(!belongs_to(&no_isin, "metso-2", &outotec));
        // Neles and old Metso: other ISINs, never ours.
        let neles = news("neles", &["metso", "metso-corporation", "neles"], NELES);
        let old = news("metso", &["metso", "metso-corporation"], OLD_METSO);
        assert!(!belongs_to(&metso, "metso-2", &neles));
        assert!(!belongs_to(&metso, "metso-2", &old));
        // A registry row that wrongly points at slug `metso` claims their items by slug...
        assert!(belongs_to(&metso, "metso", &neles));
        // ...which the mapping check then refuses (see `the_neles_and_old_metso_...`).
        assert!(!mapping_problems(&metso, &neles.author_tickers, &neles.author_isins).is_empty());
    }

    #[test]
    fn an_isin_mfn_does_not_list_is_reported_with_both_values() {
        let c = company("NOVO-B", "XCSE", Some("DK0062498333"));
        let p = mapping_problems(&c, &v(&["XCSE:NOVO B"]), &v(&["SE0008348304"]));
        assert_eq!(p.len(), 1);
        assert!(
            p[0].contains("DK0062498333") && p[0].contains("SE0008348304"),
            "{p:?}"
        );
    }
}
