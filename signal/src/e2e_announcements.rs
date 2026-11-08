//! The signal side of the company-announcement chain against real recorded MFN
//! feed pages (NEX-240): the events the `nordic-reports` poll emits for three
//! recorded company feeds (FirstFarms da+en, Volvo sv+en, Metso fi+en) are
//! replayed, in emission order, through the same dispatch `main.rs` uses
//! (`reports::parse` then `reports::handle`).
//!
//! The `.events.pb` files are written by chronicle's `announcements_e2e` test
//! (length-delimited `SpecialEvent`s). To refresh the fixtures, follow the
//! instructions at the top of `chronicle/tests/announcements_e2e.rs` (re-record
//! the feed pages with curl and jq, then
//! `REFRESH_E2E_FIXTURES=1 cargo test -p chronicle --test announcements_e2e`),
//! and update the hand-counted constants below.
//!
//! The expected counts are literals counted by hand from the fixtures, never
//! computed with the code under test.

use crate::announcements::{self, Outcome};
use crate::reports;
use model::generated::SpecialEvent;
use prost::Message;
use rust_decimal::Decimal;
use sqlx::PgPool;
use std::str::FromStr;

const FIRSTFARMS: &[u8] =
    include_bytes!("../../chronicle/tests/fixtures/mfn/e2e/firstfarms.events.pb");
const VOLVO: &[u8] = include_bytes!("../../chronicle/tests/fixtures/mfn/e2e/volvo.events.pb");
const METSO: &[u8] = include_bytes!("../../chronicle/tests/fixtures/mfn/e2e/metso.events.pb");

/// Distinct announcement releases per company, counted by hand (each da/sv/fi
/// item and its English twin are one release):
///
/// * FirstFarms 8: profit warnings 2026-08-26 and 2025-11-20, general meeting
///   2026-03-31 and 2026-04-28, the 2025 annual report document 2026-03-25,
///   the shareholder announcement and the insider report of 2025-12-04, and a
///   second guidance cut 2026-07-08;
/// * Volvo 12: voting rights 2026-10-30 and 2026-08-31, five press releases in
///   October (19, 9, 5) and September (3, 10, 15, 23, 24: the 24th has no
///   Swedish twin), 2026-07-27 and the 2026-07-03 invitation;
/// * Metso 14: 9 managers' transactions at 2026-04-24T13:00:00Z (18 items),
///   the 2026-05-08 and 2026-05-07 notifications, the 2026-04-22 board
///   committees and annual general meeting resolutions, and the 2026-04-02
///   appointment.
const FIRSTFARMS_ANNOUNCEMENTS: i64 = 8;
const VOLVO_ANNOUNCEMENTS: i64 = 12;
const METSO_ANNOUNCEMENTS: i64 = 14;

/// Report rows (one per results release, twins merged): FirstFarms interim
/// 2026-08-26 and 2025-11-26 and the Q1 update 2026-05-27; Volvo Q3 and Q2;
/// Metso Q1.
const FIRSTFARMS_REPORTS: i64 = 3;
const VOLVO_REPORTS: i64 = 2;
const METSO_REPORTS: i64 = 1;

fn decode(mut bytes: &[u8]) -> Vec<SpecialEvent> {
    let mut out = Vec::new();
    while !bytes.is_empty() {
        out.push(SpecialEvent::decode_length_delimited(&mut bytes).expect("a valid event file"));
    }
    out
}

fn poll_events() -> Vec<SpecialEvent> {
    [FIRSTFARMS, VOLVO, METSO]
        .into_iter()
        .flat_map(decode)
        .collect()
}

async fn replay(pool: &PgPool, events: &[SpecialEvent]) {
    for e in events {
        let message = reports::parse(e).expect("the poll only emits valid events");
        reports::handle(pool, &message).await.unwrap();
    }
}

type Row = (
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Vec<String>,
);

async fn rows(pool: &PgPool) -> Vec<Row> {
    sqlx::query_as(
        "SELECT ticker, event_type, source_id, lang, \
                to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
                description, body_text, tags \
         FROM special_events WHERE status = 'occurred' \
         ORDER BY ticker, event_type, occurred_at, source_id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn count(pool: &PgPool, ticker: &str, kind: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM special_events \
         WHERE status = 'occurred' AND ticker = $1 AND event_type = $2",
    )
    .bind(ticker)
    .bind(kind)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn one_row_per_release_and_every_twin_pair_is_one_english_row(pool: PgPool) {
    replay(&pool, &poll_events()).await;
    for (ticker, announcements, reports) in [
        ("FFARMS", FIRSTFARMS_ANNOUNCEMENTS, FIRSTFARMS_REPORTS),
        ("VOLV B", VOLVO_ANNOUNCEMENTS, VOLVO_REPORTS),
        ("METSO", METSO_ANNOUNCEMENTS, METSO_REPORTS),
    ] {
        assert_eq!(
            count(&pool, ticker, "company_announcement").await,
            announcements,
            "{ticker} announcements"
        );
        let stored_reports = count(&pool, ticker, "quarterly_report").await
            + count(&pool, ticker, "annual_report").await;
        assert_eq!(stored_reports, reports, "{ticker} reports");
    }
    let not_english: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT ticker, lang FROM special_events \
         WHERE event_type = 'company_announcement' AND lang IS DISTINCT FROM 'en'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        not_english.is_empty(),
        "every release has an English version: {not_english:?}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn the_profit_warning_and_the_interim_report_are_separate_rows(pool: PgPool) {
    replay(&pool, &decode(FIRSTFARMS)).await;
    let at: Vec<(String, String)> = sqlx::query_as(
        "SELECT to_char(occurred_at AT TIME ZONE 'UTC', 'HH24:MI:SS'), event_type \
         FROM special_events \
         WHERE ticker = 'FFARMS' AND occurred_at::date = '2026-08-26' ORDER BY occurred_at",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        at,
        vec![
            ("06:15:21".to_string(), "company_announcement".to_string()),
            ("06:37:58".to_string(), "quarterly_report".to_string()),
        ]
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn replaying_twice_changes_nothing(pool: PgPool) {
    let events = poll_events();
    replay(&pool, &events).await;
    let first = rows(&pool).await;
    replay(&pool, &events).await;
    assert_eq!(rows(&pool).await, first, "reports and announcements alike");

    for e in events
        .iter()
        .filter(|e| e.event_type == "company_announcement")
    {
        let a = announcements::parse(e).unwrap();
        let (outcome, _) = announcements::record_announcement(&pool, &a).await.unwrap();
        assert!(
            matches!(outcome, Outcome::Unchanged | Outcome::Twin),
            "{} {}: {outcome}",
            e.source_id,
            e.lang
        );
    }
    assert_eq!(rows(&pool).await, first);
}

#[sqlx::test(migrations = "./migrations")]
async fn live_events_first_then_the_poll_give_the_rows_of_the_poll_alone(pool: PgPool) {
    let events = poll_events();
    replay(&pool, &events).await;
    let poll_only = rows(&pool).await;

    sqlx::query("DELETE FROM special_events")
        .execute(&pool)
        .await
        .unwrap();
    let live: Vec<SpecialEvent> = events
        .iter()
        .filter(|e| e.event_type == "company_announcement")
        .cloned()
        .map(|mut e| {
            e.text = String::new();
            e
        })
        .collect();
    replay(&pool, &live).await;
    let live_only = rows(&pool).await;
    assert!(
        live_only
            .iter()
            .filter(|r| r.1 == "company_announcement")
            .all(|r| r.6.is_none()),
        "live events carried no text"
    );
    replay(&pool, &events).await;
    let both = rows(&pool).await;
    assert_eq!(
        both, poll_only,
        "the poll fills body_text and changes nothing else"
    );
    assert!(
        both.iter()
            .filter(|r| r.1 == "company_announcement")
            .all(|r| r.6.as_deref().is_some_and(|t| !t.is_empty())),
        "body_text is filled from the poll's event"
    );
}

/// (event type, session, reaction day, base day, move).
type Reaction = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

async fn bar(pool: &PgPool, date: &str, close: &str) {
    sqlx::query(
        "INSERT INTO daily_bars (ticker, exchange_mic, date, open, high, low, close, volume, \
           currency, source, fetched_at) \
         VALUES ('FFARMS', 'XCSE', $1::date, $2::numeric, $2::numeric, $2::numeric, \
           $2::numeric, 1000, 'DKK', 'saxo', now())",
    )
    .bind(date)
    .bind(close)
    .execute(pool)
    .await
    .unwrap();
}

/// 06:15:21Z is 08:15 in Copenhagen in August, before the 09:00 open, so the
/// reaction day is the release day. The Danish holiday rows are only the range
/// markers the calendar needs (the migrations do not seed Denmark). Close 61.40 on the base day 2026-08-25 and
/// 54.20 on 2026-08-26: (54.20 - 61.40) / 61.40 x 100 = -11.7264 (4 decimals,
/// worked by hand: 7.2 / 61.4 = 0.11726384...).
#[sqlx::test(migrations = "./migrations")]
async fn the_profit_warning_gets_a_pre_open_reaction(pool: PgPool) {
    sqlx::query(
        "INSERT INTO trading_holidays (country, date, status, note) VALUES \
         ('DK','2015-01-01','closed','New Year'), ('DK','2026-12-25','closed','Christmas Day'), \
         ('DK','2027-12-31','closed','New Year''s Eve')",
    )
    .execute(&pool)
    .await
    .unwrap();
    bar(&pool, "2026-08-25", "61.400000").await;
    bar(&pool, "2026-08-26", "54.200000").await;
    replay(&pool, &decode(FIRSTFARMS)).await;
    let reactions: Vec<Reaction> = sqlx::query_as(
        "SELECT event_type, release_session, reaction_date::text, base_date::text, \
                    move_pct::text \
             FROM special_events \
             WHERE ticker = 'FFARMS' AND occurred_at::date = '2026-08-26' ORDER BY occurred_at",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(reactions.len(), 2);
    let expected = Decimal::from_str("-11.7264").unwrap();
    for (kind, session, reaction_date, base_date, move_pct) in &reactions {
        assert_eq!(session.as_deref(), Some("pre_open"), "{kind}");
        assert_eq!(reaction_date.as_deref(), Some("2026-08-26"), "{kind}");
        assert_eq!(base_date.as_deref(), Some("2026-08-25"), "{kind}");
        assert_eq!(
            Decimal::from_str(move_pct.as_deref().unwrap()).unwrap(),
            expected,
            "{kind}: a report and an announcement of one day share the move"
        );
    }
    assert_eq!(reactions[0].0, "company_announcement");
    assert_eq!(reactions[1].0, "quarterly_report");
}
