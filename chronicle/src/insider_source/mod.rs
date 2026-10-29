//! Where a country's insider transactions come from.
//!
//! [`model::nordic::Country::insider_source`] names the *kind* of source; this
//! module holds the code that fetches it. [`insider_source`] maps the kind to
//! the implementation, so no caller matches on a country code. A new country
//! either reuses a kind or adds a kind and an implementation here.

use crate::nordic::calendar::MfnClient;
use crate::nordic::db::Company;
use crate::nordic::feed::NewsItem;
use crate::nordic::insiders;
use async_trait::async_trait;
use chrono::{NaiveDate, NaiveTime, TimeZone, Utc};
use model::generated::UnifiedInsiderTransaction;
use model::nordic::InsiderSourceKind;

pub mod fi_register;

pub use fi_register::FiRegister;

/// The handles a source may need. Sources use what applies to them.
pub struct SourceContext<'a> {
    /// HTTP client for registers (FI).
    pub http: &'a reqwest::Client,
    /// MFN feed client for release-based sources; `None` where not needed.
    pub feed: Option<&'a mut MfnClient>,
    /// The FI issuer name mapped for the company (`companies.fi_issuer`).
    pub fi_issuer: Option<&'a str>,
}

/// A source of insider transactions for one company.
#[async_trait]
pub trait InsiderSource: Send + Sync {
    /// Short stable name for logs.
    fn name(&self) -> &'static str;

    /// The company's transactions published on or after `since`. Messages carry
    /// stable ids, so fetching the same range twice is harmless.
    async fn fetch(
        &self,
        ctx: &mut SourceContext<'_>,
        company: &Company,
        since: NaiveDate,
    ) -> anyhow::Result<Vec<UnifiedInsiderTransaction>>;
}

/// MAR Art. 19 managers' transaction releases read from the company's MFN feed.
///
/// `nordic-reports` reads the same feed for reports and routes these items to
/// [`insiders::build`] in the same pass; this implementation is the
/// stand-alone read (history, re-send) over the same parsing.
pub struct MarReleases;

#[async_trait]
impl InsiderSource for MarReleases {
    fn name(&self) -> &'static str {
        "mar-releases"
    }

    async fn fetch(
        &self,
        ctx: &mut SourceContext<'_>,
        company: &Company,
        since: NaiveDate,
    ) -> anyhow::Result<Vec<UnifiedInsiderTransaction>> {
        let slug = company
            .mfn_slug
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("{} has no mfn_slug", company.ticker))?;
        let feed = ctx
            .feed
            .as_deref_mut()
            .ok_or_else(|| anyhow::anyhow!("MarReleases needs an MFN feed client"))?;
        let from = Utc.from_utc_datetime(&since.and_time(NaiveTime::MIN));
        let (items, _pages) =
            crate::nordic::ingest::collect_insider_items(feed, company, slug, from).await?;
        let refs: Vec<&NewsItem> = items.iter().collect();
        Ok(insiders::build(company, &refs)
            .messages
            .into_iter()
            .map(|(_, m)| m)
            .collect())
    }
}

/// The implementation for a kind of source; `None` where a country has none.
pub fn insider_source(kind: InsiderSourceKind) -> Option<&'static dyn InsiderSource> {
    match kind {
        InsiderSourceKind::FiRegister => Some(&FiRegister),
        InsiderSourceKind::MarReleases => Some(&MarReleases),
        InsiderSourceKind::None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_kind_maps_to_its_implementation() {
        assert_eq!(
            insider_source(InsiderSourceKind::FiRegister)
                .unwrap()
                .name(),
            "fi-register"
        );
        assert_eq!(
            insider_source(InsiderSourceKind::MarReleases)
                .unwrap()
                .name(),
            "mar-releases"
        );
        assert!(insider_source(InsiderSourceKind::None).is_none());
    }

    #[test]
    fn every_country_kind_resolves_as_declared() {
        for c in model::nordic::COUNTRIES {
            assert_eq!(
                insider_source(c.insider_source).is_some(),
                c.insider_source != InsiderSourceKind::None,
                "{}",
                c.code
            );
        }
    }
}
