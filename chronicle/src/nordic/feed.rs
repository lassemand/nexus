//! Client for MFN's JSON news feed, `https://mfn.se/all/a/<slug>.json`.
//!
//! Note: MFN's `robots.txt` disallows `*.json`; fetching these URLs was
//! explicitly approved by the repository owner (2026-10-21). Load is one
//! request per company per daily run (a handful on the first page set) and a
//! single full paging pass per company for `--backfill`.
//!
//! `publish_date` is RFC 3339 UTC and is used as-is; session classification
//! happens later in SQL, in Europe/Stockholm.

use super::calendar::MfnClient;
use anyhow::{anyhow, Context};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

/// Page size requested on the first page; later pages follow `next_url`.
pub const PAGE_SIZE: usize = 48;

#[derive(Debug, Deserialize)]
struct RawFeed {
    #[serde(default)]
    items: Option<Vec<RawItem>>,
    #[serde(default)]
    next_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawItem {
    news_id: Uuid,
    #[serde(default)]
    url: Option<String>,
    author: RawAuthor,
    #[serde(default)]
    properties: RawProperties,
    content: RawContent,
}

#[derive(Debug, Deserialize)]
struct RawAuthor {
    #[serde(default)]
    slug: String,
    #[serde(default)]
    slugs: Option<Vec<String>>,
    #[serde(default)]
    tickers: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
struct RawProperties {
    #[serde(default)]
    lang: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct RawContent {
    #[serde(default)]
    title: String,
    publish_date: String,
}

/// One news item, reduced to the fields we use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewsItem {
    /// Stable MFN item id.
    pub news_id: Uuid,
    /// Public URL of the item.
    pub url: Option<String>,
    /// `sv` / `en`, when known.
    pub lang: Option<String>,
    /// MFN tags, e.g. `sub:report:interim`.
    pub tags: Vec<String>,
    /// Title, including the issuer prefix.
    pub title: String,
    /// Publication instant (UTC).
    pub published_at: DateTime<Utc>,
    /// `author.slug`.
    pub author_slug: String,
    /// `author.slugs`.
    pub author_slugs: Vec<String>,
    /// `author.tickers`, e.g. `FNSE:GOMX`.
    pub author_tickers: Vec<String>,
}

impl NewsItem {
    /// True if the item was authored by the company with this MFN slug.
    pub fn is_authored_by(&self, slug: &str) -> bool {
        self.author_slug == slug || self.author_slugs.iter().any(|s| s == slug)
    }
}

/// One page of the feed.
#[derive(Debug)]
pub struct FeedPage {
    /// Items, newest first. Items with an unparseable date are dropped (and
    /// counted in `skipped`).
    pub items: Vec<NewsItem>,
    /// Items that could not be parsed.
    pub skipped: usize,
    /// URL of the next page, if any.
    pub next_url: Option<reqwest::Url>,
}

/// The first page URL: `<base>/all/a/<slug>.json?limit=48&offset=0`.
pub fn first_page_url(base: &reqwest::Url, slug: &str) -> anyhow::Result<reqwest::Url> {
    let mut url = base.clone();
    url.path_segments_mut()
        .map_err(|_| anyhow!("base url cannot be a base"))?
        .pop_if_empty()
        .push("all")
        .push("a")
        .push(&format!("{slug}.json"));
    url.query_pairs_mut()
        .append_pair("limit", &PAGE_SIZE.to_string())
        .append_pair("offset", "0");
    Ok(url)
}

fn same_origin(a: &reqwest::Url, b: &reqwest::Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// Fetch and parse one feed page. `next_url` must stay on the client's origin.
pub async fn fetch_feed_page(
    client: &mut MfnClient,
    url: reqwest::Url,
) -> anyhow::Result<FeedPage> {
    let base = client.base()?;
    let body = client.get_text(url.clone()).await?;
    let raw: RawFeed =
        serde_json::from_str(&body).with_context(|| format!("decoding MFN feed {}", url.path()))?;

    let mut items = Vec::new();
    let mut skipped = 0;
    for r in raw.items.unwrap_or_default() {
        match DateTime::parse_from_rfc3339(&r.content.publish_date) {
            Ok(t) => items.push(NewsItem {
                news_id: r.news_id,
                url: r.url,
                lang: r.properties.lang,
                tags: r.properties.tags.unwrap_or_default(),
                title: r.content.title,
                published_at: t.with_timezone(&Utc),
                author_slug: r.author.slug,
                author_slugs: r.author.slugs.unwrap_or_default(),
                author_tickers: r.author.tickers.unwrap_or_default(),
            }),
            Err(_) => skipped += 1,
        }
    }
    let next_url = match raw.next_url.filter(|u| !u.is_empty()) {
        Some(u) => {
            let next = reqwest::Url::parse(&u).context("bad next_url")?;
            if !same_origin(&base, &next) {
                return Err(anyhow!("next_url {next} is not on {base}"));
            }
            Some(next)
        }
        None => None,
    };
    Ok(FeedPage {
        items,
        skipped,
        next_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_page_url_uses_path_segments() {
        let base = reqwest::Url::parse("https://mfn.se").unwrap();
        assert_eq!(
            first_page_url(&base, "gomspace-group").unwrap().as_str(),
            "https://mfn.se/all/a/gomspace-group.json?limit=48&offset=0"
        );
        assert_eq!(
            first_page_url(&base, "a/../b").unwrap().path(),
            "/all/a/a%2F..%2Fb.json"
        );
    }

    #[test]
    fn authorship_uses_slug_or_slugs() {
        let mut i = NewsItem {
            news_id: Uuid::nil(),
            url: None,
            lang: None,
            tags: vec![],
            title: String::new(),
            published_at: Utc::now(),
            author_slug: "volvo".into(),
            author_slugs: vec!["volvo".into(), "volvo-group".into()],
            author_tickers: vec![],
        };
        assert!(i.is_authored_by("volvo"));
        assert!(i.is_authored_by("volvo-group"));
        assert!(!i.is_authored_by("hexagon"));
        i.author_slug = "other".into();
        i.author_slugs.clear();
        assert!(!i.is_authored_by("volvo"));
    }
}
