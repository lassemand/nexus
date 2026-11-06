//! Which MFN items count as company announcements (NEX-235).
//!
//! Everything a company publishes that is not a results report, a report
//! correction, a calendar notice or a financial-calendar release. Shared by the
//! poll and `nordic-live`; it depends on neither.

use super::calendar_release::parse_calendar_release;
use super::classify::Classification;
use super::feed::NewsItem;

/// True if an item with classification `c` is a company announcement, given
/// whether it is a financial-calendar release. Use this when the release was
/// already parsed, so it is parsed once per item.
pub fn is_announcement_class(c: &Classification, is_calendar_release: bool) -> bool {
    c.report_kind.is_none() && !c.is_correction && !c.calendar_update && !is_calendar_release
}

/// True exactly when `item` is not a results report, a report correction, a
/// calendar notice or a financial-calendar release. Regulatory or not, every
/// other item counts, including Finnish managers' transactions.
pub fn is_company_announcement(item: &NewsItem, c: &Classification) -> bool {
    is_announcement_class(c, parse_calendar_release(item).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nordic::classify::classify_item;
    use chrono::{TimeZone, Utc};

    fn item(tags: &[&str], title: &str, text: Option<&str>) -> NewsItem {
        NewsItem {
            news_id: uuid::Uuid::nil(),
            url: None,
            lang: Some("en".into()),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            title: title.into(),
            published_at: Utc.with_ymd_and_hms(2026, 8, 26, 6, 15, 21).unwrap(),
            author_name: None,
            author_slug: "x".into(),
            author_slugs: vec![],
            author_tickers: vec![],
            author_isins: vec![],
            source: None,
            attachment_urls: vec![],
            text: text.map(String::from),
        }
    }

    fn check(i: &NewsItem) -> bool {
        is_company_announcement(i, &classify_item(&i.tags, &i.title))
    }

    #[test]
    fn an_interim_report_is_not_an_announcement() {
        let i = item(
            &["sub:report:interim"],
            "Acme A/S: Interim report for Q2 2026",
            None,
        );
        assert!(!check(&i));
    }

    #[test]
    fn a_correction_is_not_an_announcement() {
        let i = item(
            &["sub:report:interim"],
            "Acme A/S: Correction: Interim report for Q2 2026",
            None,
        );
        assert!(!check(&i));
    }

    #[test]
    fn a_calendar_notice_is_not_an_announcement() {
        let i = item(
            &[":regulatory"],
            "Acme A/S: Change to the financial calendar for 2026",
            None,
        );
        let c = classify_item(&i.tags, &i.title);
        assert!(c.calendar_update);
        assert!(!is_company_announcement(&i, &c));
    }

    #[test]
    fn a_calendar_release_is_not_an_announcement() {
        let i = item(
            &[],
            "Acme A/S: Financial calendar 2026",
            Some("Annual general meeting 15 April 2026\nInterim report Q1 2026: 20 May 2026"),
        );
        let c = classify_item(&i.tags, &i.title);
        assert!(parse_calendar_release(&i).is_some());
        assert!(!is_company_announcement(&i, &c));
    }

    #[test]
    fn a_regulatory_profit_warning_is_an_announcement() {
        let i = item(
            &[":regulatory"],
            "Acme A/S: Downward adjustment of guidance for 2026",
            Some("The board lowers its outlook."),
        );
        assert!(check(&i));
    }

    #[test]
    fn an_untagged_press_release_is_an_announcement() {
        let i = item(&[], "Acme A/S launches a new product", None);
        assert!(check(&i));
    }

    #[test]
    fn a_finnish_managers_transaction_is_an_announcement() {
        let i = item(
            &[":regulatory"],
            "Acme Oyj: Johtohenkilöiden liiketoimet - Matti Meikäläinen",
            None,
        );
        assert!(crate::nordic::insiders::is_managers_transaction(&i));
        assert!(check(&i));
    }
}
