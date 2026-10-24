//! Nordic reports as `SpecialEvent`s on `special.events` (NEX-171).
//!
//! chronicle only fetches, classifies and publishes; signal stores the events
//! in `special_events` and applies the matching rules. Every message for a
//! company is keyed `"{ticker}:{exchange_mic}"` ([`company_key`]), so one
//! company's events stay in order on one partition.

use super::calendar::CalendarEntry;
use super::classify::Classification;
use super::db::Company;
use super::feed::NewsItem;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use model::generated::{ReportCalendar, ScheduledReport, SpecialEvent};
use prost::Message;
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::ClientConfig;
use std::time::Duration;

/// Default topic signal consumes special events from.
pub const SPECIAL_EVENTS_TOPIC: &str = "special.events";

/// `source` of every event published here.
pub const SOURCE_MFN: &str = "mfn";

/// The Kafka key of every message about `company`.
pub fn company_key(company: &Company) -> String {
    format!("{}:{}", company.ticker, company.exchange_mic)
}

/// Where events go: Kafka in production ([`KafkaSink`]), a vector in tests.
#[async_trait]
pub trait EventSink {
    /// Delivers one event; returns once the broker acknowledged it.
    async fn send(&self, key: &str, event: &SpecialEvent) -> anyhow::Result<()>;
}

/// Publishes `event` about `company`, keyed by [`company_key`].
pub async fn publish(
    sink: &dyn EventSink,
    company: &Company,
    event: &SpecialEvent,
) -> anyhow::Result<()> {
    sink.send(&company_key(company), event).await
}

/// Publishes to a Kafka topic.
pub struct KafkaSink {
    producer: FutureProducer,
    topic: String,
}

impl KafkaSink {
    /// Connects a producer for `topic` on `brokers`.
    pub fn new(brokers: &str, topic: impl Into<String>) -> anyhow::Result<Self> {
        let producer = ClientConfig::new()
            .set("bootstrap.servers", brokers)
            .set("message.timeout.ms", "10000")
            .create()?;
        Ok(Self {
            producer,
            topic: topic.into(),
        })
    }
}

#[async_trait]
impl EventSink for KafkaSink {
    async fn send(&self, key: &str, event: &SpecialEvent) -> anyhow::Result<()> {
        let payload = event.encode_to_vec();
        self.producer
            .send(
                FutureRecord::to(&self.topic).key(key).payload(&payload),
                Duration::from_secs(10),
            )
            .await
            .map_err(|(e, _)| anyhow::anyhow!("publishing {} for {key}: {e}", event.event_type))?;
        Ok(())
    }
}

/// The event for an MFN item, or `None` for items signal doesn't store:
/// periodic reports become `quarterly_report` / `annual_report`, corrections
/// `report_correction`; everything else (business news, calendar notices,
/// report documents) is not published.
pub fn release_event(
    company: &Company,
    item: &NewsItem,
    c: &Classification,
) -> Option<SpecialEvent> {
    let (event_type, report_kind) = if c.is_correction {
        ("report_correction", None)
    } else if c.calendar_update {
        return None;
    } else {
        let kind = c.report_kind?;
        let event_type = match kind.as_str() {
            "annual" => "annual_report",
            _ => "quarterly_report",
        };
        (event_type, Some(kind.as_str()))
    };
    Some(SpecialEvent {
        ticker: company.ticker.clone(),
        exchange_mic: company.exchange_mic.clone(),
        event_type: event_type.into(),
        occurred_at_unix_secs: item.published_at.timestamp(),
        description: item.title.clone(),
        source: SOURCE_MFN.into(),
        source_id: item.news_id.to_string(),
        url: item.url.clone().unwrap_or_default(),
        lang: item.lang.clone().unwrap_or_default(),
        tags: item.tags.clone(),
        report_kind: report_kind.unwrap_or_default().into(),
        fiscal_period: c.fiscal_period.clone().unwrap_or_default(),
        is_correction: c.is_correction,
        company_name: company.name.clone().unwrap_or_default(),
        calendar: None,
    })
}

fn scheduled(e: &CalendarEntry) -> ScheduledReport {
    ScheduledReport {
        expected_date: e.date.format("%Y-%m-%d").to_string(),
        expected_time: e
            .time
            .map(|t| t.format("%H:%M").to_string())
            .unwrap_or_default(),
        calendar_label: e.label.clone(),
        report_kind: e.kind.as_str().into(),
        fiscal_period: e.fiscal_period.clone().unwrap_or_default(),
    }
}

/// A `report_calendar` snapshot of `entries` (possibly empty) for `company`.
pub fn calendar_event(
    company: &Company,
    fetched_at: DateTime<Utc>,
    entries: &[CalendarEntry],
) -> SpecialEvent {
    SpecialEvent {
        ticker: company.ticker.clone(),
        exchange_mic: company.exchange_mic.clone(),
        event_type: "report_calendar".into(),
        occurred_at_unix_secs: fetched_at.timestamp(),
        source: SOURCE_MFN.into(),
        company_name: company.name.clone().unwrap_or_default(),
        calendar: Some(ReportCalendar {
            fetched_at_unix_secs: fetched_at.timestamp(),
            entries: entries.iter().map(scheduled).collect(),
        }),
        ..Default::default()
    }
}

/// Hash of a calendar's entries for change detection: FNV-1a 64 over a sorted,
/// field-separated serialisation, so it is stable across runs, entry order and
/// Rust versions. `fetched_at` is not part of it.
pub fn calendar_hash(entries: &[CalendarEntry]) -> String {
    let mut lines: Vec<String> = entries
        .iter()
        .map(scheduled)
        .map(|s| {
            [
                s.expected_date,
                s.expected_time,
                s.calendar_label,
                s.report_kind,
                s.fiscal_period,
            ]
            .join("\u{1f}")
        })
        .collect();
    lines.sort();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in lines.join("\u{1e}").bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nordic::classify::ReportKind;
    use chrono::{NaiveDate, NaiveTime, TimeZone};
    use uuid::Uuid;

    fn company() -> Company {
        Company {
            ticker: "GOMX".into(),
            exchange_mic: "FNSE".into(),
            mfn_slug: Some("gomspace-group".into()),
            name: Some("GomSpace Group AB (GOMX)".into()),
        }
    }

    fn item() -> NewsItem {
        NewsItem {
            news_id: Uuid::from_u128(7),
            url: Some("https://mfn.se/x".into()),
            lang: Some("en".into()),
            tags: vec!["sub:report:interim".into()],
            title: "GomSpace: Interim report Q3".into(),
            published_at: Utc.with_ymd_and_hms(2026, 11, 12, 6, 30, 0).unwrap(),
            author_slug: "gomspace-group".into(),
            author_slugs: vec![],
            author_tickers: vec!["FNSE:GOMX".into()],
        }
    }

    fn class(kind: Option<ReportKind>, correction: bool, calendar: bool) -> Classification {
        Classification {
            report_kind: kind,
            is_correction: correction,
            calendar_update: calendar,
            fiscal_period: kind.map(|_| "2026-Q3".into()),
        }
    }

    fn entry(date: (i32, u32, u32), time: Option<(u32, u32)>, label: &str) -> CalendarEntry {
        CalendarEntry {
            date: NaiveDate::from_ymd_opt(date.0, date.1, date.2).unwrap(),
            time: time.map(|(h, m)| NaiveTime::from_hms_opt(h, m, 0).unwrap()),
            label: label.into(),
            kind: ReportKind::Interim,
            fiscal_period: None,
        }
    }

    #[test]
    fn only_reports_and_corrections_are_published() {
        let c = company();
        let e =
            release_event(&c, &item(), &class(Some(ReportKind::Interim), false, false)).unwrap();
        assert_eq!(
            (
                e.event_type.as_str(),
                e.report_kind.as_str(),
                e.source.as_str()
            ),
            ("quarterly_report", "interim", "mfn")
        );
        assert_eq!(e.source_id, Uuid::from_u128(7).to_string());
        assert_eq!(e.description, "GomSpace: Interim report Q3");
        assert_eq!(e.fiscal_period, "2026-Q3");
        assert_eq!(e.company_name, "GomSpace Group AB (GOMX)");

        let annual = release_event(&c, &item(), &class(Some(ReportKind::Annual), false, false));
        assert_eq!(annual.unwrap().event_type, "annual_report");
        let fix = release_event(&c, &item(), &class(None, true, false)).unwrap();
        assert_eq!(
            (fix.event_type.as_str(), fix.is_correction),
            ("report_correction", true)
        );
        assert!(release_event(&c, &item(), &class(None, false, false)).is_none());
        assert!(release_event(&c, &item(), &class(None, false, true)).is_none());
    }

    #[test]
    fn calendar_hash_ignores_order_and_fetch_time_but_not_content() {
        let a = entry((2026, 11, 12), Some((7, 30)), "Kvartalsrapport 2026-Q3");
        let b = entry((2027, 2, 18), None, "Bokslutskommuniké 2026");
        let h = calendar_hash(&[a.clone(), b.clone()]);
        assert_eq!(h, calendar_hash(&[b.clone(), a.clone()]));
        let moved = entry((2026, 11, 14), Some((7, 30)), "Kvartalsrapport 2026-Q3");
        assert_ne!(h, calendar_hash(&[moved, b]));
        assert_ne!(calendar_hash(&[]), h);
        // Pinned: a changed algorithm would re-publish every calendar once.
        assert_eq!(calendar_hash(&[]), "cbf29ce484222325");
    }

    #[test]
    fn every_message_for_a_company_has_the_same_key() {
        let c = company();
        let fetched = Utc.with_ymd_and_hms(2026, 10, 24, 4, 30, 0).unwrap();
        let events = [
            release_event(&c, &item(), &class(Some(ReportKind::Interim), false, false)).unwrap(),
            release_event(&c, &item(), &class(None, true, false)).unwrap(),
            calendar_event(
                &c,
                fetched,
                &[entry((2026, 11, 12), None, "Kvartalsrapport")],
            ),
        ];
        for e in &events {
            assert_eq!(format!("{}:{}", e.ticker, e.exchange_mic), company_key(&c));
        }
        assert_eq!(company_key(&c), "GOMX:FNSE");
    }
}
