//! Upcoming report dates from each company's MFN calendar (NEX-161).
//!
//! Only the public company HTML page `https://mfn.se/a/<slug>` is fetched
//! (MFN's `robots.txt` allows it; the JSON/RSS/XML feeds are disallowed and not
//! used): once a day per company, plus at most once per `nordic-reports` run
//! when the company has just announced a calendar change.
//!
//! Upcoming reports are published as one `report_calendar` snapshot per
//! company ([`refresh_company`]); signal turns them into scheduled rows.

use super::classify::ReportKind;
use super::db::{self, Company};
use super::events::{calendar_event, calendar_hash, publish, EventSink};
use anyhow::{anyhow, Context};
use chrono::{NaiveDate, NaiveTime, Utc};
use scraper::{Html, Selector};
use sqlx::PgPool;
use std::time::Duration;
use thiserror::Error;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

/// Production base URL.
pub const MFN_BASE_URL: &str = "https://mfn.se";

const MAX_ATTEMPTS: u32 = 3;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(30);

/// Event labels that are known not to be reports (matched case-insensitively
/// as substrings); they are ignored silently.
const IGNORED_LABELS: &[&str] = &[
    "årsstämma",
    "bolagsstämma",
    "x-dag",
    "utdelning",
    "kapitalmarknadsdag",
    "capital markets day",
    "analytiker",
    "split",
    "annual general meeting",
];

/// (needle, kind) pairs for report labels, Swedish and English, lower-case.
const REPORT_LABELS: &[(&str, ReportKind)] = &[
    ("kvartalsrapport", ReportKind::Interim),
    ("delårsrapport", ReportKind::Interim),
    ("halvårsrapport", ReportKind::Interim),
    ("interim report", ReportKind::Interim),
    ("quarterly report", ReportKind::Interim),
    ("half-year report", ReportKind::Interim),
    ("bokslutskommuniké", ReportKind::Annual),
    ("årsredovisning", ReportKind::Annual),
    ("year-end report", ReportKind::Annual),
    ("annual report", ReportKind::Annual),
];

/// One upcoming report from the calendar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarEntry {
    /// Expected release date (`model::nordic::MFN_TIMEZONE`).
    pub date: NaiveDate,
    /// Estimated release time (`model::nordic::MFN_TIMEZONE`); `None` when MFN shows `-`.
    pub time: Option<NaiveTime>,
    /// Original label, e.g. `Kvartalsrapport 2026-Q3`.
    pub label: String,
    /// Interim or annual.
    pub kind: ReportKind,
    /// `2026-Q3` or `2026`, when the label has one.
    pub fiscal_period: Option<String>,
}

/// The calendar page could not be understood. The existing rows must be kept.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    /// The page has no `table-calender` table.
    #[error("calendar table not found on page")]
    NoTable,
    /// The table exists but no row could be parsed.
    #[error("calendar table has no parseable rows")]
    NoRows,
}

/// How a calendar event label is treated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelClass {
    /// A periodic report.
    Report(ReportKind, Option<String>),
    /// A known non-report event (AGM, ex-dividend date, …).
    Ignored,
    /// A wording we don't recognise.
    Unknown,
}

/// Classify an event label such as `Kvartalsrapport 2026-Q2`.
pub fn classify_label(label: &str) -> LabelClass {
    let lower = label.to_lowercase();
    if let Some((_, kind)) = REPORT_LABELS.iter().find(|(n, _)| lower.contains(n)) {
        return LabelClass::Report(*kind, fiscal_period(label));
    }
    if IGNORED_LABELS.iter().any(|n| lower.contains(n)) {
        return LabelClass::Ignored;
    }
    LabelClass::Unknown
}

/// `2026-Q3` or a bare four-digit year found in the label.
fn fiscal_period(label: &str) -> Option<String> {
    let mut year = None;
    for tok in label.split_whitespace() {
        let b = tok.as_bytes();
        let digits = |s: &[u8]| s.len() == 4 && s.iter().all(u8::is_ascii_digit);
        if b.len() == 7 && digits(&b[..4]) && b[4] == b'-' && (b[5] == b'Q' || b[5] == b'q') {
            if (b'1'..=b'4').contains(&b[6]) {
                return Some(format!("{}-Q{}", &tok[..4], b[6] as char));
            }
        } else if digits(b) && year.is_none() {
            year = Some(tok.to_string());
        }
    }
    year
}

fn parse_time(cell: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(cell.trim(), "%H:%M").ok()
}

/// Parse the calendar page and return the **upcoming report** rows.
///
/// Rows with class `grey-background` are past; rows without it are upcoming.
/// Non-report events are dropped; unknown upcoming labels are logged at `warn`.
/// An empty result on a correctly parsed table is valid ("nothing announced").
pub fn parse_calendar(html: &str, slug: &str) -> Result<Vec<CalendarEntry>, ParseError> {
    // Selectors are constant literals, so parsing cannot fail.
    let table_sel = Selector::parse("table.table-calender").expect("valid selector");
    let row_sel = Selector::parse("tbody tr").expect("valid selector");
    let cell_sel = Selector::parse("td").expect("valid selector");

    let doc = Html::parse_document(html);
    let table = doc.select(&table_sel).next().ok_or(ParseError::NoTable)?;

    let mut parsed_rows = 0;
    let mut upcoming = Vec::new();
    for row in table.select(&row_sel) {
        let cells: Vec<String> = row
            .select(&cell_sel)
            .map(|c| {
                c.text()
                    .flat_map(str::split_whitespace)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        let [date, time, label] = match cells.as_slice() {
            [d, t, l] => [d, t, l],
            _ => continue,
        };
        let Ok(date) = NaiveDate::parse_from_str(date, "%Y-%m-%d") else {
            continue;
        };
        parsed_rows += 1;
        if row.value().classes().any(|c| c == "grey-background") {
            continue;
        }
        match classify_label(label) {
            LabelClass::Report(kind, fiscal_period) => upcoming.push(CalendarEntry {
                date,
                time: parse_time(time),
                label: label.clone(),
                kind,
                fiscal_period,
            }),
            LabelClass::Ignored => {}
            LabelClass::Unknown => warn!(slug, label = %label, %date, "unknown MFN calendar label"),
        }
    }
    if parsed_rows == 0 {
        return Err(ParseError::NoRows);
    }
    Ok(upcoming)
}

/// Polite HTTP client for MFN company pages.
pub struct MfnClient {
    http: reqwest::Client,
    base_url: String,
    min_interval: Duration,
    last_request: Option<Instant>,
}

impl MfnClient {
    /// `min_interval` is the minimum gap between requests (2 s in production).
    pub fn new(base_url: impl Into<String>, min_interval: Duration) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("nexus-chronicle/nordic-calendar (lasse.alm@gsfleet.io)")
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.into(),
            min_interval,
            last_request: None,
        })
    }

    async fn throttle(&mut self) {
        if let Some(last) = self.last_request {
            let elapsed = last.elapsed();
            if elapsed < self.min_interval {
                tokio::time::sleep(self.min_interval - elapsed).await;
            }
        }
        self.last_request = Some(Instant::now());
    }

    /// `<base>/a/<slug>`, built with path segments so the slug cannot inject anything.
    fn page_url(&self, slug: &str) -> anyhow::Result<reqwest::Url> {
        let mut url = reqwest::Url::parse(&self.base_url)?;
        url.path_segments_mut()
            .map_err(|_| anyhow!("base url cannot be a base"))?
            .pop_if_empty()
            .push("a")
            .push(slug);
        Ok(url)
    }

    /// The base URL (`https://mfn.se` in production).
    pub fn base(&self) -> anyhow::Result<reqwest::Url> {
        Ok(reqwest::Url::parse(&self.base_url)?)
    }

    /// Fetch the company page: one request, plus waits and retries only for
    /// `429`, honouring `Retry-After`.
    pub async fn fetch_page(&mut self, slug: &str) -> anyhow::Result<String> {
        let url = self.page_url(slug)?;
        self.get_text(url).await
    }

    /// GET a URL politely: spaced by `min_interval`, 20 s timeout, and on `429`
    /// waits `Retry-After` (capped at 120 s) before retrying. Any other
    /// non-success status is an error.
    pub async fn get_text(&mut self, url: reqwest::Url) -> anyhow::Result<String> {
        for attempt in 1..=MAX_ATTEMPTS {
            self.throttle().await;
            let resp = self.http.get(url.clone()).send().await?;
            let status = resp.status();
            if status.is_success() {
                return Ok(resp.text().await?);
            }
            if status.as_u16() == 429 && attempt < MAX_ATTEMPTS {
                let wait = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .map(Duration::from_secs)
                    .unwrap_or(DEFAULT_RETRY_AFTER)
                    .min(MAX_RETRY_AFTER);
                warn!(path = url.path(), ?wait, "MFN returned 429, waiting");
                tokio::time::sleep(wait).await;
                continue;
            }
            return Err(anyhow!("MFN returned {status} for {}", url.path()));
        }
        Err(anyhow!("MFN rate limited {}", url.path()))
    }
}

/// Result of [`refresh_company`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    /// The company has no `mfn_slug`; nothing was fetched.
    NoSlug,
    /// The calendar is the same as the last snapshot published; not re-sent.
    Unchanged,
    /// A snapshot with this many upcoming reports was published.
    Published(usize),
}

/// Fetch and parse one company's MFN calendar and publish it as a
/// `report_calendar` snapshot, unless it equals the last snapshot published
/// (`mfn_ingest_state.calendar_hash`) and `force` is off. The hash is stored
/// only after the broker acknowledged the snapshot.
///
/// A fetch or parse failure is returned as an error (a [`ParseError`] is the
/// source for page changes) **after** logging it at `error`; nothing is
/// published and the stored hash is kept. Used by the daily `nordic-calendar`
/// job and by `nordic-reports` after a calendar announcement.
pub async fn refresh_company(
    pool: &PgPool,
    client: &mut MfnClient,
    sink: &dyn EventSink,
    company: &Company,
    force: bool,
) -> anyhow::Result<Refresh> {
    let Some(slug) = company.mfn_slug.as_deref() else {
        return Ok(Refresh::NoSlug);
    };
    let ticker = company.ticker.as_str();
    let html = client
        .fetch_page(slug)
        .await
        .with_context(|| format!("fetching MFN calendar for {ticker}"))?;
    let entries = match parse_calendar(&html, slug) {
        Ok(e) => e,
        Err(e) => {
            error!(ticker, slug, error = %e, "MFN calendar parse failure, nothing published");
            return Err(anyhow::Error::new(e).context(format!("parsing MFN calendar for {ticker}")));
        }
    };

    let hash = calendar_hash(&entries);
    let state = db::load_state(pool, ticker, &company.exchange_mic).await?;
    if !force && state.calendar_hash.as_deref() == Some(hash.as_str()) {
        debug!(ticker, "calendar unchanged, not re-published");
        return Ok(Refresh::Unchanged);
    }
    publish(
        sink,
        company,
        &calendar_event(company, Utc::now(), &entries),
    )
    .await?;
    db::set_calendar_hash(pool, ticker, &company.exchange_mic, &hash).await?;
    info!(ticker, upcoming = entries.len(), "calendar published");
    Ok(Refresh::Published(entries.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOMSPACE: &str = include_str!("../../tests/fixtures/mfn_gomspace.html");
    const HEXAGON: &str = include_str!("../../tests/fixtures/mfn_hexagon.html");
    const NOVO: &str = include_str!("../../tests/fixtures/mfn_novo.html");

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn gomspace_upcoming_only() {
        let rows = parse_calendar(GOMSPACE, "gomspace-group").unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.date, d("2027-02-18"));
        assert_eq!(r.time, Some(NaiveTime::from_hms_opt(8, 30, 0).unwrap()));
        assert_eq!(r.label, "Bokslutskommuniké 2026");
        assert_eq!(r.kind, ReportKind::Annual);
        assert_eq!(r.fiscal_period.as_deref(), Some("2026"));
    }

    #[test]
    fn novo_nordisk_upcoming_quarterly_report_in_the_evening() {
        // `https://mfn.se/a/novo-nordisk`: the same Swedish MFN labels as for
        // Swedish issuers, an estimated time after the Copenhagen close.
        let rows = parse_calendar(NOVO, "novo-nordisk").unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        let r = &rows[0];
        assert_eq!(r.date, d("2026-11-04"));
        assert_eq!(r.time, Some(NaiveTime::from_hms_opt(18, 35, 0).unwrap()));
        assert_eq!(r.label, "Kvartalsrapport 2026-Q3");
        assert_eq!(r.kind, ReportKind::Interim);
        assert_eq!(r.fiscal_period.as_deref(), Some("2026-Q3"));
    }

    #[test]
    fn hexagon_large_cap_upcoming_quarters_and_annual() {
        let rows = parse_calendar(HEXAGON, "hexagon").unwrap();
        let got: Vec<_> = rows
            .iter()
            .map(|r| (r.date.to_string(), r.kind, r.fiscal_period.clone().unwrap()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("2027-04-29".into(), ReportKind::Interim, "2027-Q1".into()),
                ("2027-01-28".into(), ReportKind::Annual, "2026".into()),
                ("2026-10-23".into(), ReportKind::Interim, "2026-Q3".into()),
            ]
        );
    }

    #[test]
    fn dash_time_is_null_and_non_reports_are_ignored() {
        let html = page(&[
            ("", "2027-03-01", "-", "Kvartalsrapport 2027-Q1"),
            ("", "2027-03-02", "", "Årsstämma"),
            (
                "",
                "2027-03-03",
                "08:00",
                "X-dag ordinarie utdelning GOMX 0.00 SEK",
            ),
            ("", "2027-03-04", "08:00", "Extra Bolagsstämma 2027"),
            ("", "2027-03-05", "08:00", "Kapitalmarknadsdag 2027"),
        ]);
        let rows = parse_calendar(&html, "x").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].time, None);
    }

    #[test]
    fn english_labels_and_kinds() {
        for (label, kind, period) in [
            (
                "Interim report 2027-Q2",
                ReportKind::Interim,
                Some("2027-Q2"),
            ),
            (
                "Quarterly Report 2027-Q3",
                ReportKind::Interim,
                Some("2027-Q3"),
            ),
            ("Half-year report 2027", ReportKind::Interim, Some("2027")),
            ("Year-end report 2026", ReportKind::Annual, Some("2026")),
            ("Annual report 2026", ReportKind::Annual, Some("2026")),
            ("Delårsrapport", ReportKind::Interim, None),
            (
                "Halvårsrapport 2027-Q2",
                ReportKind::Interim,
                Some("2027-Q2"),
            ),
            ("Årsredovisning 2026", ReportKind::Annual, Some("2026")),
        ] {
            assert_eq!(
                classify_label(label),
                LabelClass::Report(kind, period.map(String::from)),
                "{label}"
            );
        }
        assert_eq!(
            classify_label("Annual General Meeting 2027"),
            LabelClass::Ignored
        );
        assert_eq!(
            classify_label("Webcast för investerare"),
            LabelClass::Unknown
        );
        assert_eq!(classify_label("15-10 2023-Q3"), LabelClass::Unknown);
    }

    #[test]
    fn empty_upcoming_list_is_valid() {
        let html = page(&[(
            "grey-background",
            "2026-08-26",
            "-",
            "Kvartalsrapport 2026-Q2",
        )]);
        assert_eq!(parse_calendar(&html, "x").unwrap(), vec![]);
    }

    #[test]
    fn missing_table_or_rows_is_a_parse_failure() {
        assert_eq!(
            parse_calendar("<html><body><h1>Redesigned</h1></body></html>", "x"),
            Err(ParseError::NoTable)
        );
        let empty = page(&[]);
        assert_eq!(parse_calendar(&empty, "x"), Err(ParseError::NoRows));
        let junk = page(&[("", "not-a-date", "-", "Kvartalsrapport 2027-Q1")]);
        assert_eq!(parse_calendar(&junk, "x"), Err(ParseError::NoRows));
    }

    #[test]
    fn page_url_uses_path_segments() {
        let c = MfnClient::new("https://mfn.se", Duration::ZERO).unwrap();
        assert_eq!(
            c.page_url("gomspace-group").unwrap().as_str(),
            "https://mfn.se/a/gomspace-group"
        );
        assert_eq!(c.page_url("a/../b?x").unwrap().path(), "/a/a%2F..%2Fb%3Fx");
    }

    fn page(rows: &[(&str, &str, &str, &str)]) -> String {
        let body: String = rows
            .iter()
            .map(|(class, date, time, label)| {
                format!("<tr class=\"{class}\"><td>{date}</td><td>{time}</td><td>{label}</td></tr>")
            })
            .collect();
        format!(
            "<html><body><table class=\"table-calender\"><thead><tr><td></td><td>Est. tid*</td><td></td></tr></thead><tbody>{body}</tbody></table></body></html>"
        )
    }
}
