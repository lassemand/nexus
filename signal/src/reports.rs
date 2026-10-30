//! Nordic periodic reports in `special_events` (NEX-171 / NEX-172).
//!
//! chronicle publishes one `SpecialEvent` per MFN item (`source = "mfn"`) and
//! one `report_calendar` snapshot per company calendar; this module is the
//! only place reports are stored. The rules are those chronicle used for its
//! former `report_releases` table:
//!
//! * an MFN item is stored once (`(source, source_id)`), its display fields
//!   refreshed on re-delivery;
//! * the other-language twin of a release (same company and type, same instant
//!   or, for a different `lang` with the same tags, within [`TWIN_WINDOW_SECS`])
//!   is merged, English preferred;
//! * a release fills the closest scheduled row within ±[`MATCH_WINDOW_DAYS`]
//!   of its Stockholm date, keeping the expected fields;
//! * a same-day re-send becomes a `report_correction`, and every report links
//!   to its latest correction within 7 days (`superseded_by`);
//! * a calendar snapshot replaces the company's future scheduled rows.
//!
//! Delivery is at least once and only ordered per company (the producer keys
//! by `ticker:exchange_mic`), so every rule is idempotent and gives the same
//! rows whatever the duplicate count.

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use model::generated::SpecialEvent;
use sqlx::{PgConnection, PgPool, Row};
use thiserror::Error;
use tracing::info;

/// `source` of events published by chronicle's Nordic jobs.
pub const SOURCE_MFN: &str = "mfn";

/// Days either side of a release's Stockholm date within which a scheduled
/// report counts as the same report.
pub const MATCH_WINDOW_DAYS: i32 = 7;

/// Seconds within which items of different `lang` with the same tags are
/// translations of one release (MFN publishes some twins 23-30 s apart).
pub const TWIN_WINDOW_SECS: i32 = 120;

const QUARTERLY: &str = "quarterly_report";
const ANNUAL: &str = "annual_report";
const CORRECTION: &str = "report_correction";
const CALENDAR: &str = "report_calendar";

/// Why a message was rejected. Logged and skipped (offset committed): a
/// redelivery would be rejected the same way.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum Invalid {
    #[error("empty ticker")]
    NoTicker,
    #[error("empty exchange_mic")]
    NoExchange,
    #[error("empty source_id")]
    NoSourceId,
    #[error("unknown event_type {0:?}")]
    EventType(String),
    #[error("unknown report_kind {0:?}")]
    ReportKind(String),
    #[error("invalid occurred_at {0}")]
    Timestamp(i64),
    #[error("report_calendar without a calendar")]
    NoCalendar,
    #[error("calendar entry with invalid {0}")]
    CalendarEntry(&'static str),
}

/// A released item (report or correction), validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release<'a> {
    pub ticker: &'a str,
    pub exchange_mic: &'a str,
    pub event_type: &'static str,
    pub occurred_at: DateTime<Utc>,
    pub source_id: &'a str,
    pub description: Option<&'a str>,
    pub url: Option<&'a str>,
    pub lang: Option<&'a str>,
    pub tags: &'a [String],
    pub report_kind: Option<&'static str>,
    pub fiscal_period: Option<&'a str>,
    pub is_correction: bool,
    pub company_name: Option<&'a str>,
}

impl Release<'_> {
    /// A results release (not a correction): may fill a scheduled row.
    fn is_report(&self) -> bool {
        !self.is_correction && self.event_type != CORRECTION
    }
}

/// One upcoming report from a calendar snapshot, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scheduled {
    pub expected_date: NaiveDate,
    pub expected_time: Option<NaiveTime>,
    pub calendar_label: String,
    pub report_kind: &'static str,
    pub fiscal_period: Option<String>,
}

/// A company's calendar snapshot, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Calendar<'a> {
    pub ticker: &'a str,
    pub exchange_mic: &'a str,
    pub company_name: Option<&'a str>,
    pub fetched_at: DateTime<Utc>,
    pub entries: Vec<Scheduled>,
}

/// A decoded `source = "mfn"` message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message<'a> {
    Release(Release<'a>),
    Calendar(Calendar<'a>),
}

fn non_empty(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
}

fn report_kind(raw: &str, event_type: &str) -> Result<Option<&'static str>, Invalid> {
    match (raw, event_type) {
        ("interim", _) | ("", QUARTERLY) => Ok(Some("interim")),
        ("annual", _) | ("", ANNUAL) => Ok(Some("annual")),
        ("", _) => Ok(None),
        (other, _) => Err(Invalid::ReportKind(other.to_string())),
    }
}

fn timestamp(secs: i64) -> Result<DateTime<Utc>, Invalid> {
    DateTime::from_timestamp(secs, 0).ok_or(Invalid::Timestamp(secs))
}

/// Validates a `source = "mfn"` event.
pub fn parse(e: &SpecialEvent) -> Result<Message<'_>, Invalid> {
    let ticker = non_empty(&e.ticker).ok_or(Invalid::NoTicker)?;
    let exchange_mic = non_empty(&e.exchange_mic).ok_or(Invalid::NoExchange)?;
    let company_name = non_empty(&e.company_name);

    if e.event_type == CALENDAR {
        let cal = e.calendar.as_ref().ok_or(Invalid::NoCalendar)?;
        let entries = cal
            .entries
            .iter()
            .map(|s| {
                let expected_date = NaiveDate::parse_from_str(&s.expected_date, "%Y-%m-%d")
                    .map_err(|_| Invalid::CalendarEntry("expected_date"))?;
                let expected_time = match s.expected_time.as_str() {
                    "" => None,
                    t => Some(
                        NaiveTime::parse_from_str(t, "%H:%M")
                            .map_err(|_| Invalid::CalendarEntry("expected_time"))?,
                    ),
                };
                let report_kind = match s.report_kind.as_str() {
                    "interim" => "interim",
                    "annual" => "annual",
                    _ => return Err(Invalid::CalendarEntry("report_kind")),
                };
                Ok(Scheduled {
                    expected_date,
                    expected_time,
                    calendar_label: s.calendar_label.clone(),
                    report_kind,
                    fiscal_period: non_empty(&s.fiscal_period).map(str::to_string),
                })
            })
            .collect::<Result<_, _>>()?;
        return Ok(Message::Calendar(Calendar {
            ticker,
            exchange_mic,
            company_name,
            fetched_at: timestamp(cal.fetched_at_unix_secs)?,
            entries,
        }));
    }

    let event_type = match e.event_type.as_str() {
        QUARTERLY => QUARTERLY,
        ANNUAL => ANNUAL,
        CORRECTION => CORRECTION,
        other => return Err(Invalid::EventType(other.to_string())),
    };
    Ok(Message::Release(Release {
        ticker,
        exchange_mic,
        event_type,
        occurred_at: timestamp(e.occurred_at_unix_secs)?,
        source_id: non_empty(&e.source_id).ok_or(Invalid::NoSourceId)?,
        description: non_empty(&e.description),
        url: non_empty(&e.url),
        lang: non_empty(&e.lang),
        tags: &e.tags,
        report_kind: report_kind(&e.report_kind, event_type)?,
        fiscal_period: non_empty(&e.fiscal_period),
        is_correction: e.is_correction || event_type == CORRECTION,
        company_name,
    }))
}

/// What [`record_report_release`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// The item was already stored; display fields refreshed.
    Updated,
    /// The other-language twin of a stored release; no new row.
    Merged,
    /// Filled a scheduled row; carries its `expected_date`.
    Matched(NaiveDate),
    /// A new row.
    Inserted,
}

/// Stores one released MFN item, in one transaction (see the module docs).
pub async fn record_report_release(pool: &PgPool, r: &Release<'_>) -> sqlx::Result<Recorded> {
    let mut tx = pool.begin().await?;
    let recorded = record_in(&mut tx, r).await?;
    if matches!(recorded, Recorded::Inserted | Recorded::Matched(_)) {
        flag_resends(&mut tx, r.ticker, r.exchange_mic).await?;
    }
    link_corrections(&mut tx, r.ticker, r.exchange_mic).await?;
    tx.commit().await?;
    Ok(recorded)
}

async fn record_in(tx: &mut PgConnection, r: &Release<'_>) -> sqlx::Result<Recorded> {
    // 1. Known item: refresh the display fields only.
    let updated = sqlx::query(
        "UPDATE special_events SET description = $3, url = $4, lang = $5, tags = $6, \
                company_name = COALESCE($7, company_name) \
         WHERE source = $1 AND source_id = $2 RETURNING id",
    )
    .bind(SOURCE_MFN)
    .bind(r.source_id)
    .bind(r.description)
    .bind(r.url)
    .bind(r.lang)
    .bind(r.tags)
    .bind(r.company_name)
    .fetch_optional(&mut *tx)
    .await?;
    if updated.is_some() {
        return Ok(Recorded::Updated);
    }

    // 2. Language twin: same company and type, at the same instant or, in
    // another language with the same tags, within TWIN_WINDOW_SECS. A report that was
    // re-typed as a same-day re-send still matches its own twins through
    // report_kind, so the order of arrival doesn't matter.
    let twin = sqlx::query(
        "SELECT id, lang FROM special_events \
         WHERE ticker = $1 AND exchange_mic = $2 AND status = 'occurred' \
           AND (occurred_at = $3 OR ( \
                occurred_at BETWEEN $3 - make_interval(secs => $7) \
                                AND $3 + make_interval(secs => $7) \
                AND lang IS NOT NULL AND $8::text IS NOT NULL AND lang <> $8 \
                AND tags @> $9 AND tags <@ $9)) \
           AND (event_type = $4 OR (event_type = $5 AND report_kind IS NOT DISTINCT FROM $6)) \
         ORDER BY abs(extract(epoch FROM occurred_at - $3)), id LIMIT 1 FOR UPDATE",
    )
    .bind(r.ticker)
    .bind(r.exchange_mic)
    .bind(r.occurred_at)
    .bind(r.event_type)
    .bind(CORRECTION)
    .bind(r.report_kind)
    .bind(f64::from(TWIN_WINDOW_SECS))
    .bind(r.lang)
    .bind(r.tags)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(row) = twin {
        let stored_lang: Option<String> = row.get("lang");
        if stored_lang.as_deref() != Some("en") && r.lang == Some("en") {
            sqlx::query(
                "UPDATE special_events SET source_id = $2, description = $3, url = $4, \
                        lang = $5, tags = $6 WHERE id = $1",
            )
            .bind(row.get::<i64, _>("id"))
            .bind(r.source_id)
            .bind(r.description)
            .bind(r.url)
            .bind(r.lang)
            .bind(r.tags)
            .execute(&mut *tx)
            .await?;
        }
        return Ok(Recorded::Merged);
    }

    // 3. A results release fills the closest scheduled row (ties: earlier date).
    if r.is_report() {
        let candidate = sqlx::query(concat!(
            "SELECT id, expected_date FROM special_events \
             WHERE ticker = $1 AND exchange_mic = $2 AND status = 'scheduled' \
               AND expected_date BETWEEN \
                   (($3::timestamptz AT TIME ZONE '",
            model::mfn_timezone!(),
            "')::date - $4::int) AND \
                   (($3::timestamptz AT TIME ZONE '",
            model::mfn_timezone!(),
            "')::date + $4::int) \
             ORDER BY abs(expected_date - ($3::timestamptz AT TIME ZONE '",
            model::mfn_timezone!(),
            "')::date), \
                      expected_date \
             LIMIT 1 FOR UPDATE"
        ))
        .bind(r.ticker)
        .bind(r.exchange_mic)
        .bind(r.occurred_at)
        .bind(MATCH_WINDOW_DAYS)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = candidate {
            let expected: NaiveDate = row.get("expected_date");
            sqlx::query(
                "UPDATE special_events SET status = 'occurred', occurred_at = $2, \
                        event_type = $3, source = $4, source_id = $5, description = $6, \
                        url = $7, lang = $8, tags = $9, report_kind = $10, \
                        fiscal_period = COALESCE($11, fiscal_period), \
                        company_name = COALESCE($12, company_name) \
                 WHERE id = $1",
            )
            .bind(row.get::<i64, _>("id"))
            .bind(r.occurred_at)
            .bind(r.event_type)
            .bind(SOURCE_MFN)
            .bind(r.source_id)
            .bind(r.description)
            .bind(r.url)
            .bind(r.lang)
            .bind(r.tags)
            .bind(r.report_kind)
            .bind(r.fiscal_period)
            .bind(r.company_name)
            .execute(&mut *tx)
            .await?;
            info!(ticker = r.ticker, expected_date = %expected, occurred_at = %r.occurred_at,
                  "matched scheduled report");
            return Ok(Recorded::Matched(expected));
        }
    }

    // 4. A new row.
    let inserted = sqlx::query(
        "INSERT INTO special_events \
           (ticker, exchange_mic, event_type, occurred_at, description, status, source, \
            source_id, url, lang, tags, report_kind, fiscal_period, is_correction, company_name) \
         VALUES ($1,$2,$3,$4,$5,'occurred',$6,$7,$8,$9,$10,$11,$12,$13,$14) \
         ON CONFLICT DO NOTHING",
    )
    .bind(r.ticker)
    .bind(r.exchange_mic)
    .bind(r.event_type)
    .bind(r.occurred_at)
    .bind(r.description)
    .bind(SOURCE_MFN)
    .bind(r.source_id)
    .bind(r.url)
    .bind(r.lang)
    .bind(r.tags)
    .bind(r.report_kind)
    .bind(r.fiscal_period)
    .bind(r.is_correction)
    .bind(r.company_name)
    .execute(&mut *tx)
    .await?;
    Ok(if inserted.rows_affected() == 1 {
        Recorded::Inserted
    } else {
        // Raced with the same item or its twin on another connection.
        Recorded::Merged
    })
}

/// Same-day re-sends: a report on the same Stockholm date as an *earlier*
/// report of the same company becomes a `report_correction` (the earliest
/// instant stays the release the market reacted to). Idempotent.
pub async fn flag_resends(conn: &mut PgConnection, ticker: &str, mic: &str) -> sqlx::Result<u64> {
    let res = sqlx::query(concat!(
        "UPDATE special_events r SET is_correction = true, event_type = $3 \
         WHERE r.ticker = $1 AND r.exchange_mic = $2 AND r.status = 'occurred' \
           AND r.event_type IN ($4, $5) AND NOT r.is_correction \
           AND EXISTS ( \
               SELECT 1 FROM special_events e \
               WHERE e.ticker = r.ticker AND e.exchange_mic = r.exchange_mic \
                 AND e.status = 'occurred' AND e.event_type IN ($4, $5) AND NOT e.is_correction \
                 AND e.occurred_at < r.occurred_at \
                 AND (e.occurred_at AT TIME ZONE '",
        model::mfn_timezone!(),
        "')::date \
                     = (r.occurred_at AT TIME ZONE '",
        model::mfn_timezone!(),
        "')::date) \
           AND NOT EXISTS ( \
               SELECT 1 FROM special_events c \
               WHERE c.ticker = r.ticker AND c.exchange_mic = r.exchange_mic \
                 AND c.status = 'occurred' AND c.event_type = $3 \
                 AND c.occurred_at = r.occurred_at)"
    ))
    .bind(ticker)
    .bind(mic)
    .bind(CORRECTION)
    .bind(QUARTERLY)
    .bind(ANNUAL)
    .execute(conn)
    .await?;
    Ok(res.rows_affected())
}

/// Points each report's `superseded_by` at the latest correction of the same
/// company published within 7 days after it. Idempotent.
pub async fn link_corrections(
    conn: &mut PgConnection,
    ticker: &str,
    mic: &str,
) -> sqlx::Result<u64> {
    let res = sqlx::query(
        "UPDATE special_events o SET superseded_by = c.id \
         FROM ( \
             SELECT DISTINCT ON (o2.id) o2.id AS original_id, c2.id AS id \
             FROM special_events o2 \
             JOIN special_events c2 \
               ON c2.ticker = o2.ticker AND c2.exchange_mic = o2.exchange_mic \
              AND c2.status = 'occurred' AND c2.is_correction \
              AND c2.occurred_at > o2.occurred_at \
              AND c2.occurred_at <= o2.occurred_at + interval '7 days' \
             WHERE o2.ticker = $1 AND o2.exchange_mic = $2 AND o2.status = 'occurred' \
               AND o2.event_type IN ($3, $4) AND NOT o2.is_correction \
             ORDER BY o2.id, c2.occurred_at DESC \
         ) c \
         WHERE o.id = c.original_id AND o.superseded_by IS DISTINCT FROM c.id",
    )
    .bind(ticker)
    .bind(mic)
    .bind(QUARTERLY)
    .bind(ANNUAL)
    .execute(conn)
    .await?;
    Ok(res.rows_affected())
}

/// What [`apply_report_calendar`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CalendarOutcome {
    /// The snapshot was older than one already applied, and ignored.
    pub stale: bool,
    /// Future scheduled rows removed before applying.
    pub replaced: u64,
    /// Scheduled rows inserted.
    pub scheduled: usize,
    /// Entries that filled the expected fields of an occurred report.
    pub matched_occurred: usize,
}

/// Applies a company's calendar snapshot in one transaction.
///
/// Ignored if older than the newest snapshot already applied for the company.
/// Otherwise the company's scheduled rows with `expected_date >= today`
/// (Stockholm) are deleted, and each entry either fills the expected fields of
/// an occurred report within ±[`MATCH_WINDOW_DAYS`] whose `expected_date` is
/// NULL (closest wins, ties the earlier) or becomes a scheduled row. Other
/// occurred rows are never changed; past scheduled rows ("expected, not seen")
/// stay. An empty snapshot clears the future scheduled rows.
pub async fn apply_report_calendar(
    pool: &PgPool,
    c: &Calendar<'_>,
    today: NaiveDate,
) -> sqlx::Result<CalendarOutcome> {
    let mut tx = pool.begin().await?;
    let mut out = CalendarOutcome::default();

    // Serialise snapshots of one company against each other.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(format!("calendar:{}:{}", c.ticker, c.exchange_mic))
        .execute(&mut *tx)
        .await?;
    let newest: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT max(calendar_fetched_at) FROM special_events WHERE ticker = $1 AND exchange_mic = $2",
    )
    .bind(c.ticker)
    .bind(c.exchange_mic)
    .fetch_one(&mut *tx)
    .await?;
    if newest.is_some_and(|n| n > c.fetched_at) {
        out.stale = true;
        tx.commit().await?;
        return Ok(out);
    }

    out.replaced = sqlx::query(
        "DELETE FROM special_events \
         WHERE ticker = $1 AND exchange_mic = $2 AND status = 'scheduled' AND expected_date >= $3",
    )
    .bind(c.ticker)
    .bind(c.exchange_mic)
    .bind(today)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    for e in &c.entries {
        let occurred = sqlx::query(concat!(
            "SELECT id FROM special_events \
             WHERE ticker = $1 AND exchange_mic = $2 AND status = 'occurred' \
               AND event_type IN ($5, $6) AND NOT is_correction AND expected_date IS NULL \
               AND (occurred_at AT TIME ZONE '",
            model::mfn_timezone!(),
            "')::date \
                   BETWEEN ($3::date - $4::int) AND ($3::date + $4::int) \
             ORDER BY abs((occurred_at AT TIME ZONE '",
            model::mfn_timezone!(),
            "')::date - $3::date), \
                      occurred_at \
             LIMIT 1 FOR UPDATE"
        ))
        .bind(c.ticker)
        .bind(c.exchange_mic)
        .bind(e.expected_date)
        .bind(MATCH_WINDOW_DAYS)
        .bind(QUARTERLY)
        .bind(ANNUAL)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = occurred {
            sqlx::query(
                "UPDATE special_events SET expected_date = $2, expected_time = $3, \
                        calendar_label = $4, fiscal_period = COALESCE(fiscal_period, $5), \
                        calendar_fetched_at = $6 \
                 WHERE id = $1",
            )
            .bind(row.get::<i64, _>("id"))
            .bind(e.expected_date)
            .bind(e.expected_time)
            .bind(&e.calendar_label)
            .bind(&e.fiscal_period)
            .bind(c.fetched_at)
            .execute(&mut *tx)
            .await?;
            out.matched_occurred += 1;
            continue;
        }
        let event_type = if e.report_kind == "annual" {
            ANNUAL
        } else {
            QUARTERLY
        };
        out.scheduled += sqlx::query(concat!(
            "INSERT INTO special_events \
               (ticker, exchange_mic, event_type, occurred_at, description, status, source, \
                report_kind, fiscal_period, company_name, expected_date, expected_time, \
                calendar_label, calendar_fetched_at) \
             VALUES ($1, $2, $3, \
                     ($4::date + COALESCE($5::time, TIME '00:00')) AT TIME ZONE '",
            model::mfn_timezone!(),
            "', \
                     $6, 'scheduled', $7, $8, $9, $10, $4, $5, $6, $11) \
             ON CONFLICT DO NOTHING"
        ))
        .bind(c.ticker)
        .bind(c.exchange_mic)
        .bind(event_type)
        .bind(e.expected_date)
        .bind(e.expected_time)
        .bind(&e.calendar_label)
        .bind(SOURCE_MFN)
        .bind(e.report_kind)
        .bind(&e.fiscal_period)
        .bind(c.company_name)
        .bind(c.fetched_at)
        .execute(&mut *tx)
        .await?
        .rows_affected() as usize;
    }
    tx.commit().await?;
    Ok(out)
}

/// Today's date in the MFN calendar's time zone (`model::nordic::MFN_TIMEZONE`), from the database clock.
pub async fn stockholm_today(pool: &PgPool) -> sqlx::Result<NaiveDate> {
    sqlx::query_scalar(concat!(
        "SELECT (now() AT TIME ZONE '",
        model::mfn_timezone!(),
        "')::date"
    ))
    .fetch_one(pool)
    .await
}

/// Stores a validated message: a release, or a calendar snapshot.
pub async fn handle(pool: &PgPool, m: &Message<'_>) -> sqlx::Result<String> {
    match m {
        Message::Release(r) => {
            let recorded = record_report_release(pool, r).await?;
            // The release may be new, moved, or have turned another report into a
            // correction: recompute the company's reactions (NEX-163). A failure
            // here is repaired by the next bar, report or the startup sweep and
            // must not make the consumer re-store the event.
            if let Err(e) =
                crate::reactions::refresh_report_reactions(pool, r.ticker, r.exchange_mic).await
            {
                tracing::warn!(ticker = r.ticker, error = %e, "failed to refresh report reactions");
            }
            Ok(format!("{recorded:?}"))
        }
        Message::Calendar(c) => {
            let today = stockholm_today(pool).await?;
            let outcome = apply_report_calendar(pool, c, today).await?;
            Ok(format!("{outcome:?}"))
        }
    }
}

/// Builds a `ReportCalendar` event; used by tests.
#[cfg(test)]
pub fn calendar_event(
    ticker: &str,
    mic: &str,
    fetched_at: DateTime<Utc>,
    entries: Vec<model::generated::ScheduledReport>,
) -> SpecialEvent {
    SpecialEvent {
        ticker: ticker.into(),
        exchange_mic: mic.into(),
        event_type: CALENDAR.into(),
        source: SOURCE_MFN.into(),
        calendar: Some(model::generated::ReportCalendar {
            fetched_at_unix_secs: fetched_at.timestamp(),
            entries,
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use model::generated::ScheduledReport;

    /// ticker, event_type, status, occurred_at, lang, has expected_date, has superseded_by
    type SnapshotRow = (String, String, String, String, Option<String>, bool, bool);

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    /// An MFN release event for GOMX/FNSE.
    fn item(id: &str, event_type: &str, when: DateTime<Utc>, lang: &str) -> SpecialEvent {
        SpecialEvent {
            ticker: "GOMX".into(),
            exchange_mic: "FNSE".into(),
            event_type: event_type.into(),
            occurred_at_unix_secs: when.timestamp(),
            description: format!("headline {id}"),
            source: SOURCE_MFN.into(),
            source_id: id.into(),
            url: format!("https://mfn.se/{id}"),
            lang: lang.into(),
            tags: vec![":regulatory".into()],
            is_correction: event_type == CORRECTION,
            company_name: "GomSpace Group AB (GOMX)".into(),
            ..Default::default()
        }
    }

    fn entry(date: &str, time: &str, label: &str, kind: &str) -> ScheduledReport {
        ScheduledReport {
            expected_date: date.into(),
            expected_time: time.into(),
            calendar_label: label.into(),
            report_kind: kind.into(),
            fiscal_period: String::new(),
        }
    }

    async fn store(pool: &PgPool, e: &SpecialEvent) -> String {
        handle(pool, &parse(e).unwrap()).await.unwrap()
    }

    async fn apply(pool: &PgPool, e: &SpecialEvent, today: NaiveDate) -> CalendarOutcome {
        match parse(e).unwrap() {
            Message::Calendar(c) => apply_report_calendar(pool, &c, today).await.unwrap(),
            Message::Release(_) => panic!("not a calendar"),
        }
    }

    async fn schedule(pool: &PgPool, date: &str) {
        sqlx::query(
            "INSERT INTO special_events (ticker, exchange_mic, event_type, occurred_at, status, \
               source, report_kind, expected_date, expected_time, calendar_label) \
             VALUES ('GOMX','FNSE','quarterly_report', ($1::date + TIME '08:00') AT TIME ZONE \
               'Europe/Stockholm', 'scheduled', 'mfn', 'interim', $1::date, '08:00', 'Delårsrapport')",
        )
        .bind(date)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn rows(
        pool: &PgPool,
    ) -> Vec<(String, String, String, Option<String>, Option<NaiveDate>)> {
        sqlx::query_as(
            "SELECT event_type, status, to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI'), \
                    lang, expected_date FROM special_events ORDER BY occurred_at, event_type",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 24).unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn same_item_twice_is_one_row(pool: PgPool) {
        let e = item("a", QUARTERLY, at(2026, 8, 26, 6, 0), "en");
        assert_eq!(store(&pool, &e).await, "Inserted");
        assert_eq!(store(&pool, &e).await, "Updated");
        assert_eq!(rows(&pool).await.len(), 1);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn twins_merge_to_english_in_either_order(pool: PgPool) {
        let sv = item("sv", QUARTERLY, at(2026, 8, 26, 6, 0), "sv");
        let en = item("en", QUARTERLY, at(2026, 8, 26, 6, 0), "en");
        for order in [[&sv, &en], [&en, &sv]] {
            sqlx::query("DELETE FROM special_events")
                .execute(&pool)
                .await
                .unwrap();
            for e in order {
                store(&pool, e).await;
            }
            let (id, lang): (String, String) =
                sqlx::query_as("SELECT source_id, lang FROM special_events")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!((id.as_str(), lang.as_str()), ("en", "en"));
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn twins_published_seconds_apart_merge_to_english(pool: PgPool) {
        let da = item("da", QUARTERLY, at(2026, 8, 26, 5, 29), "da");
        let mut en = item("en", QUARTERLY, at(2026, 8, 26, 5, 29), "en");
        en.occurred_at_unix_secs += 27;
        for order in [[&da, &en], [&en, &da]] {
            sqlx::query("DELETE FROM special_events")
                .execute(&pool)
                .await
                .unwrap();
            for e in order {
                store(&pool, e).await;
            }
            let rows: Vec<(String, String, bool)> =
                sqlx::query_as("SELECT source_id, event_type, is_correction FROM special_events")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            assert_eq!(rows, vec![("en".to_string(), QUARTERLY.to_string(), false)]);
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn same_language_items_seconds_apart_are_not_twins(pool: PgPool) {
        let a = item("a", QUARTERLY, at(2026, 8, 26, 5, 29), "en");
        let mut b = item("b", QUARTERLY, at(2026, 8, 26, 5, 29), "en");
        b.occurred_at_unix_secs += 27;
        store(&pool, &a).await;
        store(&pool, &b).await;
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM special_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 2);
    }

    /// Metso's Q3 2026 release as MFN published it: one Finnish and one English
    /// item at the same instant, with different ids.
    fn metso_q3(id: &str, lang: &str, title: &str) -> SpecialEvent {
        SpecialEvent {
            ticker: "METSO".into(),
            exchange_mic: "XHEL".into(),
            event_type: QUARTERLY.into(),
            occurred_at_unix_secs: at(2026, 10, 22, 6, 0).timestamp(),
            description: title.into(),
            source: SOURCE_MFN.into(),
            source_id: id.into(),
            lang: lang.into(),
            tags: vec![
                ":regulatory".into(),
                "sub:report".into(),
                "sub:report:interim".into(),
            ],
            report_kind: "interim".into(),
            fiscal_period: "2026-Q3".into(),
            company_name: "Metso".into(),
            ..Default::default()
        }
    }

    const METSO_FI: (&str, &str) = (
        "d4850232-0000-0000-0000-000000000001",
        "Metson osavuosikatsaus 1. tammikuuta - 30. syyskuuta 2026",
    );
    const METSO_EN: (&str, &str) = (
        "48d2ed81-0000-0000-0000-000000000002",
        "Metso's Interim Report January 1 - September 30, 2026",
    );

    #[sqlx::test(migrations = "./migrations")]
    async fn finnish_and_english_twins_merge_to_english_in_either_order(pool: PgPool) {
        let fi = metso_q3(METSO_FI.0, "fi", METSO_FI.1);
        let en = metso_q3(METSO_EN.0, "en", METSO_EN.1);
        for order in [[&fi, &en], [&en, &fi]] {
            sqlx::query("DELETE FROM special_events")
                .execute(&pool)
                .await
                .unwrap();
            for e in order {
                store(&pool, e).await;
            }
            let (n, id, lang, title, period): (i64, String, String, String, String) =
                sqlx::query_as(
                    "SELECT count(*) OVER (), source_id, lang, description, fiscal_period \
                     FROM special_events",
                )
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(n, 1, "one row for the pair");
            assert_eq!((id.as_str(), lang.as_str()), (METSO_EN.0, "en"));
            assert_eq!(title, METSO_EN.1);
            assert_eq!(period, "2026-Q3");
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_finnish_report_without_an_english_copy_stays_a_valid_report(pool: PgPool) {
        store(&pool, &metso_q3(METSO_FI.0, "fi", METSO_FI.1)).await;
        let (event_type, status, lang, kind, period): (String, String, String, String, String) =
            sqlx::query_as(
                "SELECT event_type, status, lang, report_kind, fiscal_period FROM special_events",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            (event_type.as_str(), status.as_str(), lang.as_str()),
            (QUARTERLY, "occurred", "fi")
        );
        assert_eq!((kind.as_str(), period.as_str()), ("interim", "2026-Q3"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn twins_of_different_companies_or_instants_are_not_merged(pool: PgPool) {
        // Same instant and type, another company: its own row.
        let mut other = metso_q3(METSO_EN.0, "en", METSO_EN.1);
        other.ticker = "NESTE".into();
        store(&pool, &metso_q3(METSO_FI.0, "fi", METSO_FI.1)).await;
        store(&pool, &other).await;
        // Same company and type, another instant (the half-year report): its own row.
        let mut h1 = metso_q3(
            "h1-en",
            "en",
            "Metso's Half-Year Report January 1 - June 30, 2026",
        );
        h1.occurred_at_unix_secs = at(2026, 7, 24, 6, 0).timestamp();
        store(&pool, &h1).await;
        assert_eq!(rows(&pool).await.len(), 3);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_helsinki_calendar_row_is_stored_in_stockholm_time(pool: PgPool) {
        // MFN's calendar shows Stockholm time: 08:00 CEST is 06:00Z, not 05:00Z.
        let e = calendar_event(
            "METSO",
            "XHEL",
            at(2026, 10, 24, 4, 30),
            vec![entry(
                "2027-10-25",
                "08:00",
                "Kvartalsrapport 2027-Q3",
                "interim",
            )],
        );
        assert_eq!(apply(&pool, &e, today()).await.scheduled, 1);
        let r = rows(&pool).await;
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].2, "2027-10-25 06:00");
        assert_eq!(r[0].1, "scheduled");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn release_fills_the_scheduled_row_and_keeps_its_expected_date(pool: PgPool) {
        schedule(&pool, "2026-08-25").await;
        let e = item("q2", QUARTERLY, at(2026, 8, 26, 6, 0), "en");
        assert_eq!(store(&pool, &e).await, "Matched(2026-08-25)");
        let r = rows(&pool).await;
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].1, "occurred");
        assert_eq!(r[0].2, "2026-08-26 06:00");
        assert_eq!(r[0].4, NaiveDate::from_ymd_opt(2026, 8, 25));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn equal_distance_takes_the_earlier_scheduled_row(pool: PgPool) {
        schedule(&pool, "2026-08-23").await;
        schedule(&pool, "2026-08-29").await;
        let e = item("q2", QUARTERLY, at(2026, 8, 26, 6, 0), "en");
        assert_eq!(store(&pool, &e).await, "Matched(2026-08-23)");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_correction_never_matches_a_scheduled_row(pool: PgPool) {
        schedule(&pool, "2026-08-25").await;
        store(&pool, &item("c", CORRECTION, at(2026, 8, 26, 6, 0), "en")).await;
        let statuses: Vec<String> = rows(&pool).await.into_iter().map(|r| r.1).collect();
        assert_eq!(statuses, ["scheduled", "occurred"]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_same_day_resend_becomes_a_correction(pool: PgPool) {
        store(
            &pool,
            &item("first", QUARTERLY, at(2026, 8, 26, 7, 2), "en"),
        )
        .await;
        store(
            &pool,
            &item("again", QUARTERLY, at(2026, 8, 26, 7, 30), "en"),
        )
        .await;
        let types: Vec<String> = rows(&pool).await.into_iter().map(|r| r.0).collect();
        assert_eq!(types, [QUARTERLY, CORRECTION]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn corrections_within_seven_days_are_linked(pool: PgPool) {
        store(&pool, &item("q2", QUARTERLY, at(2026, 8, 26, 6, 0), "en")).await;
        store(&pool, &item("late", CORRECTION, at(2026, 9, 3, 6, 0), "en")).await;
        let none: Option<i64> =
            sqlx::query_scalar("SELECT superseded_by FROM special_events WHERE source_id = 'q2'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(none, None, "8 days later is too late");

        store(&pool, &item("fix", CORRECTION, at(2026, 8, 28, 6, 0), "en")).await;
        let linked: Option<String> = sqlx::query_scalar(
            "SELECT c.source_id FROM special_events o JOIN special_events c ON c.id = o.superseded_by \
             WHERE o.source_id = 'q2'",
        )
        .fetch_optional(&pool)
        .await
        .unwrap();
        assert_eq!(linked.as_deref(), Some("fix"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn legacy_earnings_are_stored_as_before(pool: PgPool) {
        let when = at(2026, 7, 30, 20, 30);
        crate::db::upsert_special_event(&pool, "AAPL", "earnings", when, None)
            .await
            .unwrap();
        crate::db::upsert_special_event(&pool, "AAPL", "earnings", when, None)
            .await
            .unwrap();
        let r: Vec<(String, String, String)> =
            sqlx::query_as("SELECT exchange_mic, status, event_type FROM special_events")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(r, [("".into(), "occurred".into(), "earnings".into())]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_calendar_snapshot_is_not_a_row_itself(pool: PgPool) {
        let empty = calendar_event("GOMX", "FNSE", at(2026, 10, 24, 4, 30), vec![]);
        assert_eq!(apply(&pool, &empty, today()).await.scheduled, 0);
        assert!(rows(&pool).await.is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn calendar_replaces_future_scheduled_rows_only(pool: PgPool) {
        let snap = |when, entries| calendar_event("GOMX", "FNSE", when, entries);
        let first = snap(
            at(2026, 10, 24, 4, 30),
            vec![
                entry("2026-11-12", "07:30", "Kvartalsrapport 2026-Q3", "interim"),
                entry("2027-02-18", "", "Bokslutskommuniké 2026", "annual"),
            ],
        );
        assert_eq!(apply(&pool, &first, today()).await.scheduled, 2);
        // A past, never-seen schedule stays.
        schedule(&pool, "2026-10-20").await;

        // MFN moves Q3 to the 14th and drops the year-end report.
        let moved = snap(
            at(2026, 10, 25, 4, 30),
            vec![entry(
                "2026-11-14",
                "07:30",
                "Kvartalsrapport 2026-Q3",
                "interim",
            )],
        );
        let out = apply(&pool, &moved, today()).await;
        assert_eq!((out.replaced, out.scheduled), (2, 1));
        let dates: Vec<Option<NaiveDate>> = rows(&pool).await.into_iter().map(|r| r.4).collect();
        assert_eq!(
            dates,
            [
                NaiveDate::from_ymd_opt(2026, 10, 20),
                NaiveDate::from_ymd_opt(2026, 11, 14)
            ]
        );
        // 07:30 Stockholm in November is 06:30 UTC; no time means midnight local.
        assert_eq!(rows(&pool).await[1].2, "2026-11-14 06:30");

        // An older snapshot arriving late is ignored.
        assert!(apply(&pool, &first, today()).await.stale);
        assert_eq!(rows(&pool).await.len(), 2);

        // An empty snapshot clears the future ones only.
        apply(&pool, &snap(at(2026, 10, 26, 4, 30), vec![]), today()).await;
        assert_eq!(rows(&pool).await.len(), 1);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn calendar_entry_fills_a_released_report_without_expected_fields(pool: PgPool) {
        store(&pool, &item("q3", QUARTERLY, at(2026, 11, 12, 6, 30), "en")).await;
        let snap = calendar_event(
            "GOMX",
            "FNSE",
            at(2026, 11, 12, 7, 0),
            vec![entry(
                "2026-11-12",
                "07:30",
                "Kvartalsrapport 2026-Q3",
                "interim",
            )],
        );
        let out = apply(&pool, &snap, NaiveDate::from_ymd_opt(2026, 11, 12).unwrap()).await;
        assert_eq!((out.matched_occurred, out.scheduled), (1, 0));
        let r = rows(&pool).await;
        assert_eq!(r.len(), 1);
        assert_eq!(
            (r[0].1.as_str(), r[0].4),
            ("occurred", NaiveDate::from_ymd_opt(2026, 11, 12))
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn duplicates_and_cross_company_interleaving_give_the_same_rows(pool: PgPool) {
        let other = |mut e: SpecialEvent| {
            e.ticker = "VOLV B".into();
            e.exchange_mic = "XSTO".into();
            e.source_id = format!("volvo-{}", e.source_id);
            e
        };
        let gomx = [
            calendar_event(
                "GOMX",
                "FNSE",
                at(2026, 8, 1, 4, 30),
                vec![entry(
                    "2026-08-25",
                    "08:00",
                    "Delårsrapport 2026-Q2",
                    "interim",
                )],
            ),
            item("sv", QUARTERLY, at(2026, 8, 26, 6, 0), "sv"),
            item("en", QUARTERLY, at(2026, 8, 26, 6, 0), "en"),
            item("again", QUARTERLY, at(2026, 8, 26, 6, 40), "en"),
            item("fix", CORRECTION, at(2026, 8, 28, 6, 0), "en"),
        ];
        let volvo: Vec<SpecialEvent> = gomx[1..].iter().cloned().map(other).collect();
        let today = NaiveDate::from_ymd_opt(2026, 8, 2).unwrap();

        let mut results = Vec::new();
        for round in 0..3 {
            sqlx::query("DELETE FROM special_events")
                .execute(&pool)
                .await
                .unwrap();
            // Per-company order is kept; companies interleave differently and
            // every message is delivered a different number of times.
            let mut queue: Vec<&SpecialEvent> = Vec::new();
            for i in 0..gomx.len().max(volvo.len()) {
                let pair = [gomx.get(i), volvo.get(i)];
                let pair = if round % 2 == 0 {
                    pair
                } else {
                    [pair[1], pair[0]]
                };
                for e in pair.into_iter().flatten() {
                    for _ in 0..=round {
                        queue.push(e);
                    }
                }
            }
            for e in queue {
                match parse(e).unwrap() {
                    Message::Calendar(c) => {
                        apply_report_calendar(&pool, &c, today).await.unwrap();
                    }
                    m => {
                        handle(&pool, &m).await.unwrap();
                    }
                }
            }
            let snapshot: Vec<SnapshotRow> = sqlx::query_as(
                "SELECT e.ticker, e.event_type, e.status, \
                            to_char(e.occurred_at AT TIME ZONE 'UTC', 'MM-DD HH24:MI'), e.lang, \
                            e.expected_date IS NOT NULL, e.superseded_by IS NOT NULL \
                     FROM special_events e ORDER BY 1, 4, 2",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            results.push(snapshot);
        }
        assert_eq!(results[0], results[1]);
        assert_eq!(results[1], results[2]);
        // GOMX: the release filled the schedule (en kept), the re-send and the
        // correction are corrections, and the release links to the latest.
        let gomx_rows: Vec<_> = results[0].iter().filter(|r| r.0 == "GOMX").collect();
        assert_eq!(gomx_rows.len(), 3);
        assert_eq!(gomx_rows[0].1, QUARTERLY);
        assert_eq!(gomx_rows[0].4.as_deref(), Some("en"));
        assert!(gomx_rows[0].5 && gomx_rows[0].6);
        assert!(gomx_rows[1..].iter().all(|r| r.1 == CORRECTION));
    }

    #[test]
    fn invalid_messages_are_rejected() {
        let ok = item("a", QUARTERLY, at(2026, 8, 26, 6, 0), "en");
        let bad = |f: fn(&mut SpecialEvent)| {
            let mut e = ok.clone();
            f(&mut e);
            parse(&e).unwrap_err()
        };
        assert_eq!(bad(|e| e.ticker.clear()), Invalid::NoTicker);
        assert_eq!(bad(|e| e.exchange_mic.clear()), Invalid::NoExchange);
        assert_eq!(bad(|e| e.source_id.clear()), Invalid::NoSourceId);
        assert_eq!(
            bad(|e| e.report_kind = "monthly".into()),
            Invalid::ReportKind("monthly".into())
        );
        assert_eq!(
            bad(|e| e.event_type = "agm".into()),
            Invalid::EventType("agm".into())
        );
        assert_eq!(
            bad(|e| e.occurred_at_unix_secs = i64::MAX),
            Invalid::Timestamp(i64::MAX)
        );
        assert_eq!(bad(|e| e.event_type = CALENDAR.into()), Invalid::NoCalendar);
        let Message::Release(r) = parse(&ok).unwrap() else {
            panic!()
        };
        assert_eq!(r.report_kind, Some("interim"));
    }
}
