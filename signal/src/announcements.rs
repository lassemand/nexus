//! Company announcements in `special_events` (NEX-236).
//!
//! chronicle publishes every non-report MFN release of a registered company as
//! a `company_announcement` event, one per language version, with the release
//! text in `text`. This module stores each as one `occurred` row:
//!
//! * an MFN item is stored once (`(source, source_id)`); a re-delivery
//!   refreshes the display fields and the text;
//! * the other-language twin (same company, a different `lang`, equal tag
//!   sets, within [`TWIN_WINDOW_SECS`]) is merged into one row, English
//!   preferred, keeping the earliest publish instant;
//! * two different announcements of the same language in the same second are
//!   two rows, and announcements never touch the report calendar.

use crate::reports::{is_uuid, non_empty, timestamp, Invalid, SOURCE_MFN, TWIN_WINDOW_SECS};
use chrono::{DateTime, Utc};
use model::generated::SpecialEvent;
use sqlx::{PgConnection, PgPool, Row};
use std::fmt;
use tracing::{info, warn};

/// `event_type` of announcement events and rows.
pub const COMPANY_ANNOUNCEMENT: &str = "company_announcement";

/// A validated `company_announcement` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announcement<'a> {
    pub ticker: &'a str,
    pub exchange_mic: &'a str,
    pub occurred_at: DateTime<Utc>,
    pub source_id: &'a str,
    pub description: Option<&'a str>,
    pub url: Option<&'a str>,
    pub lang: Option<&'a str>,
    pub tags: &'a [String],
    pub body_text: Option<&'a str>,
    pub company_name: Option<&'a str>,
}

/// What [`record_announcement`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A new row.
    Inserted,
    /// The item was stored already and its fields changed.
    Updated,
    /// The item was stored already and nothing changed.
    Unchanged,
    /// The other-language twin of a stored announcement; no new row.
    Twin,
}

impl Outcome {
    /// The outcome as logged and counted: `inserted|updated|unchanged|twin`.
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Inserted => "inserted",
            Outcome::Updated => "updated",
            Outcome::Unchanged => "unchanged",
            Outcome::Twin => "twin",
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Validates a `company_announcement` event: non-empty ticker, exchange and a
/// UUID `source_id`, a positive `occurred_at` and an empty `report_kind`. An
/// empty `text` becomes `None`.
pub fn parse(e: &SpecialEvent) -> Result<Announcement<'_>, Invalid> {
    let ticker = non_empty(&e.ticker).ok_or(Invalid::NoTicker)?;
    let exchange_mic = non_empty(&e.exchange_mic).ok_or(Invalid::NoExchange)?;
    let source_id = non_empty(&e.source_id).ok_or(Invalid::NoSourceId)?;
    if !is_uuid(source_id) {
        return Err(Invalid::SourceIdNotUuid(source_id.to_string()));
    }
    if e.occurred_at_unix_secs <= 0 {
        return Err(Invalid::Timestamp(e.occurred_at_unix_secs));
    }
    if !e.report_kind.is_empty() {
        return Err(Invalid::ReportKind(e.report_kind.clone()));
    }
    Ok(Announcement {
        ticker,
        exchange_mic,
        occurred_at: timestamp(e.occurred_at_unix_secs)?,
        source_id,
        description: non_empty(&e.description),
        url: non_empty(&e.url),
        lang: non_empty(&e.lang),
        tags: &e.tags,
        body_text: non_empty(&e.text),
        company_name: non_empty(&e.company_name),
    })
}

/// Stores one announcement in one transaction (see the module docs). Returns
/// what happened and the id of the row that now holds the announcement.
pub async fn record_announcement(
    pool: &PgPool,
    a: &Announcement<'_>,
) -> sqlx::Result<(Outcome, i64)> {
    let mut tx = pool.begin().await?;
    let outcome = record_in(&mut tx, a).await?;
    tx.commit().await?;
    Ok(outcome)
}

async fn refresh_known(
    tx: &mut PgConnection,
    a: &Announcement<'_>,
) -> sqlx::Result<Option<(Outcome, i64)>> {
    let updated = sqlx::query(
        "UPDATE special_events SET description = $3, url = $4, lang = $5, tags = $6, \
                body_text = $7, company_name = COALESCE($8, company_name) \
         WHERE source = $1 AND source_id = $2 \
           AND (description IS DISTINCT FROM $3 OR url IS DISTINCT FROM $4 \
                OR lang IS DISTINCT FROM $5 OR tags IS DISTINCT FROM $6 \
                OR body_text IS DISTINCT FROM $7 \
                OR company_name IS DISTINCT FROM COALESCE($8, company_name)) \
         RETURNING id",
    )
    .bind(SOURCE_MFN)
    .bind(a.source_id)
    .bind(a.description)
    .bind(a.url)
    .bind(a.lang)
    .bind(a.tags)
    .bind(a.body_text)
    .bind(a.company_name)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(row) = updated {
        return Ok(Some((Outcome::Updated, row.get("id"))));
    }
    let known: Option<i64> =
        sqlx::query_scalar("SELECT id FROM special_events WHERE source = $1 AND source_id = $2")
            .bind(SOURCE_MFN)
            .bind(a.source_id)
            .fetch_optional(&mut *tx)
            .await?;
    Ok(known.map(|id| (Outcome::Unchanged, id)))
}

async fn record_in(tx: &mut PgConnection, a: &Announcement<'_>) -> sqlx::Result<(Outcome, i64)> {
    if let Some(known) = refresh_known(tx, a).await? {
        return Ok(known);
    }

    let twin = sqlx::query(
        "SELECT id, lang FROM special_events \
         WHERE ticker = $1 AND exchange_mic = $2 AND status = 'occurred' \
           AND event_type = $3 \
           AND occurred_at BETWEEN $4 - make_interval(secs => $5) \
                               AND $4 + make_interval(secs => $5) \
           AND lang IS NOT NULL AND $6::text IS NOT NULL AND lang <> $6 \
           AND tags @> $7 AND tags <@ $7 \
         ORDER BY abs(extract(epoch FROM occurred_at - $4)), id LIMIT 1 FOR UPDATE",
    )
    .bind(a.ticker)
    .bind(a.exchange_mic)
    .bind(COMPANY_ANNOUNCEMENT)
    .bind(a.occurred_at)
    .bind(f64::from(TWIN_WINDOW_SECS))
    .bind(a.lang)
    .bind(a.tags)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(row) = twin {
        let stored_lang: Option<String> = row.get("lang");
        if stored_lang.as_deref() != Some("en") && a.lang == Some("en") {
            sqlx::query(
                "UPDATE special_events SET source_id = $2, description = $3, url = $4, \
                        lang = $5, tags = $6, body_text = $7 WHERE id = $1",
            )
            .bind(row.get::<i64, _>("id"))
            .bind(a.source_id)
            .bind(a.description)
            .bind(a.url)
            .bind(a.lang)
            .bind(a.tags)
            .bind(a.body_text)
            .execute(&mut *tx)
            .await?;
        }
        return Ok((Outcome::Twin, row.get("id")));
    }

    let inserted = sqlx::query(
        "INSERT INTO special_events \
           (ticker, exchange_mic, event_type, occurred_at, description, status, source, \
            source_id, url, lang, tags, report_kind, is_correction, company_name, body_text) \
         VALUES ($1,$2,$3,$4,$5,'occurred',$6,$7,$8,$9,$10,NULL,false,$11,$12) \
         ON CONFLICT (source, source_id) WHERE source_id IS NOT NULL DO NOTHING \
         RETURNING id",
    )
    .bind(a.ticker)
    .bind(a.exchange_mic)
    .bind(COMPANY_ANNOUNCEMENT)
    .bind(a.occurred_at)
    .bind(a.description)
    .bind(SOURCE_MFN)
    .bind(a.source_id)
    .bind(a.url)
    .bind(a.lang)
    .bind(a.tags)
    .bind(a.company_name)
    .bind(a.body_text)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(row) = inserted {
        return Ok((Outcome::Inserted, row.get("id")));
    }
    refresh_known(tx, a).await?.ok_or(sqlx::Error::RowNotFound)
}

/// Stores a validated announcement, logs and counts the outcome, refreshes the
/// share-price reaction of the stored row (unless it was already stored
/// unchanged) and returns the outcome as text.
pub async fn handle(pool: &PgPool, a: &Announcement<'_>) -> sqlx::Result<String> {
    let (outcome, id) = record_announcement(pool, a).await?;
    if outcome != Outcome::Unchanged {
        // A failure here is repaired by the next daily bar or the startup sweep
        // and must not make the consumer re-store the event.
        if let Err(e) = crate::reactions::refresh_rows(pool, a.ticker, a.exchange_mic, &[id]).await
        {
            warn!(ticker = a.ticker, error = %e, "failed to refresh announcement reaction");
        }
    }
    info!(
        ticker = a.ticker,
        source_id = a.source_id,
        outcome = outcome.as_str(),
        "company announcement stored"
    );
    metrics::counter!("signal_nordic_events_total", "event_type" => COMPANY_ANNOUNCEMENT, "result" => outcome.as_str()).increment(1);
    Ok(outcome.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(secs: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 26, 6, 15, 0).unwrap()
            + chrono::Duration::seconds(secs.into())
    }

    fn id(n: u32) -> String {
        format!("00000000-0000-4000-8000-{n:012}")
    }

    fn event(n: u32, lang: &str, secs: u32, tags: &[&str]) -> SpecialEvent {
        SpecialEvent {
            ticker: "GOMX".into(),
            exchange_mic: "FNSE".into(),
            event_type: COMPANY_ANNOUNCEMENT.into(),
            occurred_at_unix_secs: at(secs).timestamp(),
            description: format!("headline {n}"),
            source: SOURCE_MFN.into(),
            source_id: id(n),
            url: format!("https://mfn.se/{n}"),
            lang: lang.into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            company_name: "GomSpace Group AB (GOMX)".into(),
            text: format!("text {n}"),
            ..Default::default()
        }
    }

    async fn store(pool: &PgPool, e: &SpecialEvent) -> String {
        crate::reports::handle(pool, &crate::reports::parse(e).unwrap())
            .await
            .unwrap()
    }

    type Row = (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
    );

    async fn rows(pool: &PgPool) -> Vec<Row> {
        sqlx::query_as(
            "SELECT source_id, lang, body_text, description, \
                    to_char(occurred_at AT TIME ZONE 'UTC', 'HH24:MI:SS') \
             FROM special_events ORDER BY occurred_at, id",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    const REG: &[&str] = &[":regulatory"];

    #[test]
    fn parse_requires_the_identifying_fields() {
        let ok = event(1, "en", 0, REG);
        assert!(parse(&ok).is_ok());
        let mut e = ok.clone();
        e.source_id.clear();
        assert_eq!(parse(&e), Err(Invalid::NoSourceId));
        let mut e = ok.clone();
        e.source_id = "nope".into();
        assert!(matches!(parse(&e), Err(Invalid::SourceIdNotUuid(_))));
        let mut e = ok.clone();
        e.report_kind = "interim".into();
        assert!(matches!(parse(&e), Err(Invalid::ReportKind(_))));
        let mut e = ok.clone();
        e.occurred_at_unix_secs = 0;
        assert_eq!(parse(&e), Err(Invalid::Timestamp(0)));
        let mut e = ok.clone();
        e.ticker.clear();
        assert_eq!(parse(&e), Err(Invalid::NoTicker));
        let mut e = ok;
        e.exchange_mic.clear();
        assert_eq!(parse(&e), Err(Invalid::NoExchange));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn insert_redelivery_and_update(pool: PgPool) {
        let mut e = event(1, "en", 0, REG);
        assert_eq!(store(&pool, &e).await, "inserted");
        assert_eq!(store(&pool, &e).await, "unchanged");
        e.description = "new title".into();
        assert_eq!(store(&pool, &e).await, "updated");
        let r = rows(&pool).await;
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].3.as_deref(), Some("new title"));
        assert_eq!(r[0].2.as_deref(), Some("text 1"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn danish_then_english_merge_to_one_english_row(pool: PgPool) {
        assert_eq!(store(&pool, &event(1, "da", 0, REG)).await, "inserted");
        assert_eq!(store(&pool, &event(2, "en", 25, REG)).await, "twin");
        let r = rows(&pool).await;
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, id(2));
        assert_eq!(r[0].1.as_deref(), Some("en"));
        assert_eq!(r[0].2.as_deref(), Some("text 2"));
        assert_eq!(r[0].4, "06:15:00", "keeps the earliest instant");
        assert_eq!(store(&pool, &event(1, "da", 0, REG)).await, "twin");
        assert_eq!(store(&pool, &event(2, "en", 25, REG)).await, "unchanged");
        assert_eq!(rows(&pool).await.len(), 1);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn english_then_danish_stays_english(pool: PgPool) {
        assert_eq!(store(&pool, &event(2, "en", 0, REG)).await, "inserted");
        assert_eq!(store(&pool, &event(1, "da", 25, REG)).await, "twin");
        let r = rows(&pool).await;
        assert_eq!(r.len(), 1);
        assert_eq!(
            (r[0].0.as_str(), r[0].1.as_deref()),
            (id(2).as_str(), Some("en"))
        );
        assert_eq!(r[0].2.as_deref(), Some("text 2"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn same_language_same_second_is_two_rows(pool: PgPool) {
        assert_eq!(store(&pool, &event(1, "en", 0, REG)).await, "inserted");
        assert_eq!(store(&pool, &event(2, "en", 0, REG)).await, "inserted");
        assert_eq!(rows(&pool).await.len(), 2);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn different_tags_are_not_twins(pool: PgPool) {
        assert_eq!(store(&pool, &event(1, "da", 0, REG)).await, "inserted");
        let other = event(2, "en", 0, &[":regulatory", "sub:ci"]);
        assert_eq!(store(&pool, &other).await, "inserted");
        assert_eq!(rows(&pool).await.len(), 2);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn twins_outside_the_window_are_two_rows(pool: PgPool) {
        assert_eq!(store(&pool, &event(1, "da", 0, REG)).await, "inserted");
        assert_eq!(store(&pool, &event(2, "en", 121, REG)).await, "inserted");
        assert_eq!(rows(&pool).await.len(), 2);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_announcement_and_a_report_do_not_interact(pool: PgPool) {
        let report = SpecialEvent {
            event_type: "quarterly_report".into(),
            report_kind: "interim".into(),
            source_id: id(9),
            text: String::new(),
            ..event(9, "en", 0, REG)
        };
        assert_eq!(store(&pool, &report).await, "Inserted");
        assert_eq!(store(&pool, &event(1, "da", 0, REG)).await, "inserted");
        assert_eq!(store(&pool, &event(2, "en", 0, REG)).await, "twin");
        let types: Vec<(String,)> =
            sqlx::query_as("SELECT event_type FROM special_events ORDER BY event_type")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            types,
            vec![
                ("company_announcement".to_string(),),
                ("quarterly_report".to_string(),)
            ]
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn empty_text_is_stored_as_null(pool: PgPool) {
        let mut e = event(1, "en", 0, REG);
        e.text.clear();
        store(&pool, &e).await;
        assert_eq!(rows(&pool).await[0].2, None);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_dedup_index_still_guards_reports_only(pool: PgPool) {
        for n in [1, 2] {
            sqlx::query(
                "INSERT INTO special_events (ticker, exchange_mic, event_type, occurred_at, \
                   status, source, source_id) \
                 VALUES ('GOMX','FNSE','company_announcement', now(), 'occurred', 'mfn', $1)",
            )
            .bind(id(n))
            .execute(&pool)
            .await
            .unwrap();
        }
        let insert_report = |n: u32| {
            let pool = pool.clone();
            async move {
                sqlx::query(
                    "INSERT INTO special_events (ticker, exchange_mic, event_type, occurred_at, \
                       status, source, source_id) \
                     VALUES ('GOMX','FNSE','quarterly_report', '2026-08-26 06:00+00', \
                       'occurred', 'mfn', $1)",
                )
                .bind(id(n))
                .execute(&pool)
                .await
            }
        };
        insert_report(3).await.unwrap();
        assert!(insert_report(4).await.is_err());
    }
}
