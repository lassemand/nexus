/// Nasdaq Nordic report-release ingestion binary (NEX-160).
///
/// For every Swedish-calendar company (`FNSE`, `XSTO`) with a
/// `companies.nasdaq_news_company` mapping, pages through the exchange's
/// company-news feed and stores each item in `report_releases` with its exact
/// release instant, classified as interim/annual report, correction and/or
/// financial-calendar announcement. Non-reports are stored with
/// `report_kind = NULL` so the rule can be re-run.
///
/// Incremental: paging stops at the first page containing a known
/// `disclosure_id`. Companies that received a new calendar announcement are
/// logged (`calendar_updated`) so the MFN calendar refresh can pick them up.
mod nordic;

use clap::Parser;
use nordic::classify::{self, REPORT_CATEGORIES};
use nordic::db::{self, Company, NewRelease};
use nordic::feed::{NewsClient, NASDAQ_NEWS_URL, PAGE_SIZE};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::process::ExitCode;
use std::time::Duration;
use tracing::{error, info, warn};

/// Exchanges following the Swedish calendar / Stockholm time.
const SE_MICS: &[&str] = &["FNSE", "XSTO"];

#[derive(Parser)]
#[command(about = "Ingest past Nordic report releases from the Nasdaq Nordic company-news API")]
struct Args {
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    #[arg(long, env = "NASDAQ_NEWS_URL", default_value = NASDAQ_NEWS_URL)]
    news_url: String,
}

/// Result of ingesting one company.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CompanyOutcome {
    /// Rows newly inserted.
    pub inserted: usize,
    /// Newly inserted rows that are financial-calendar announcements.
    pub new_calendar_updates: usize,
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
            if db::insert_release(pool, &row).await? {
                outcome.inserted += 1;
                if calendar_update {
                    outcome.new_calendar_updates += 1;
                }
            } else {
                hit_known = true;
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

#[tokio::main]
async fn main() -> ExitCode {
    dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/.env")).ok();
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    match run(args).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            error!(error = %e, "nordic-reports failed");
            ExitCode::FAILURE
        }
    }
}

/// Returns `Ok(false)` only if every attempted company failed.
async fn run(args: Args) -> anyhow::Result<bool> {
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&args.database_url)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;

    info!(categories = ?REPORT_CATEGORIES, "report categories counted");
    let companies = db::load_companies(&pool, SE_MICS).await?;
    let mut client = NewsClient::new(
        args.news_url,
        Duration::from_secs(1),
        Duration::from_secs(2),
    )?;

    let (mut attempted, mut failed) = (0, 0);
    let mut calendar_updated: Vec<String> = Vec::new();
    for c in &companies {
        let Some(issuer) = c.nasdaq_news_company.as_deref() else {
            info!(ticker = %c.ticker, mic = %c.exchange_mic, "no nasdaq_news_company mapping, skipping");
            continue;
        };
        attempted += 1;
        match ingest_company(&pool, &mut client, c, issuer).await {
            Ok(o) => {
                info!(ticker = %c.ticker, inserted = o.inserted, "ingested");
                if o.new_calendar_updates > 0 {
                    calendar_updated.push(c.ticker.clone());
                }
            }
            Err(e) => {
                failed += 1;
                error!(ticker = %c.ticker, error = %e, "company failed");
            }
        }
    }
    if !calendar_updated.is_empty() {
        info!(tickers = ?calendar_updated, "calendar_updated: new financial-calendar announcements");
    }
    info!(attempted, failed, "nordic-reports done");
    Ok(!(attempted > 0 && failed == attempted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nordic::feed::NewsItem;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Fixture {
        results: R,
    }
    #[derive(Deserialize)]
    struct R {
        item: Vec<NewsItem>,
    }

    fn fixture() -> Vec<NewsItem> {
        serde_json::from_str::<Fixture>(include_str!("../tests/fixtures/nasdaq_news_gomspace.json"))
            .unwrap()
            .results
            .item
    }

    fn kind(id: i64) -> Option<&'static str> {
        let items = fixture();
        let i = items.iter().find(|i| i.disclosure_id == id).unwrap();
        classify::report_kind(&i.cns_category, &i.headline).map(|k| k.as_str())
    }

    #[test]
    fn gomspace_fixture_classification() {
        assert_eq!(kind(1460208), Some("interim")); // Q2 2026 (08:00 local)
        assert_eq!(kind(1440487), Some("interim")); // Q1 2026
        assert_eq!(kind(1430824), None); // order intake news
        assert_eq!(kind(1413953), None); // market guidance
        assert_eq!(kind(1421253), Some("annual"));
        assert_eq!(kind(1385457), Some("interim")); // original
        assert_eq!(kind(1385496), None); // its correction
        assert_eq!(kind(1373582), None); // non-report correction
        assert_eq!(kind(1312423), Some("interim")); // old category
        assert_eq!(kind(1307899), None); // market update
        assert_eq!(kind(1464054), None); // company announcement
    }

    #[test]
    fn gomspace_fixture_flags() {
        let items = fixture();
        let get = |id| items.iter().find(|i| i.disclosure_id == id).unwrap();
        assert!(classify::is_correction(&get(1385496).headline));
        assert!(classify::is_correction(&get(1373582).headline));
        assert!(classify::is_calendar_update(&get(1340291).headline));
        assert!(classify::is_calendar_update(&get(1215246).headline));
        assert!(!items
            .iter()
            .any(|i| i.disclosure_id == 1464054 && classify::is_calendar_update(&i.headline)));
        // Summer 08:00 local and winter 08:00 local are 06:00Z / 07:00Z — we get UTC directly.
        assert_eq!(
            get(1460208).released_at().unwrap().to_rfc3339(),
            "2026-08-26T06:00:00+00:00"
        );
        assert_eq!(
            get(1421253).released_at().unwrap().to_rfc3339(),
            "2026-02-19T07:00:25+00:00"
        );
    }

    /// Full ingest against an ephemeral database. Needs `TEST_DATABASE_URL`
    /// (a role allowed to CREATE DATABASE); skipped otherwise.
    #[tokio::test]
    async fn ingest_is_idempotent_and_links_corrections() {
        use sqlx::{Connection, Row};
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("TEST_DATABASE_URL not set, skipping");
            return;
        };
        let db = format!("nordic_reports_test_{}", std::process::id());
        let mut admin = sqlx::PgConnection::connect(&url).await.unwrap();
        sqlx::query(&format!("CREATE DATABASE {db}"))
            .execute(&mut admin)
            .await
            .unwrap();
        let test_url = match url.rsplit_once('/') {
            Some((base, _)) => format!("{base}/{db}"),
            None => unreachable!("database url has a path"),
        };
        let pool = PgPoolOptions::new().connect(&test_url).await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO companies (ticker, sector, exchange_mic, currency, nasdaq_news_company) \
             VALUES ('GOMX', 'Technology', 'FNSE', 'SEK', 'GomSpace Group AB')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(include_str!("../tests/fixtures/nasdaq_news_gomspace.json")),
            )
            .mount(&server)
            .await;
        let mut client =
            NewsClient::new(server.uri(), Duration::ZERO, Duration::from_millis(1)).unwrap();
        let company = &db::load_companies(&pool, SE_MICS).await.unwrap()[0];

        let first = ingest_company(&pool, &mut client, company, "GomSpace Group AB")
            .await
            .unwrap();
        assert_eq!(first.inserted, 17);
        assert_eq!(first.new_calendar_updates, 2);
        let second = ingest_company(&pool, &mut client, company, "GomSpace Group AB")
            .await
            .unwrap();
        assert_eq!(second, CompanyOutcome::default());

        let counted: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM report_releases WHERE report_kind IS NOT NULL AND NOT is_correction",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(counted >= 10, "{counted}");
        // The 2025-08-27 correction supersedes the original and is not a report itself.
        let row = sqlx::query(
            "SELECT o.superseded_by, c.id AS cid, c.report_kind, c.is_correction \
             FROM report_releases o, report_releases c \
             WHERE o.disclosure_id = 1385457 AND c.disclosure_id = 1385496",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            row.get::<Option<i64>, _>("superseded_by"),
            Some(row.get("cid"))
        );
        assert_eq!(row.get::<Option<String>, _>("report_kind"), None);
        assert!(row.get::<bool, _>("is_correction"));

        pool.close().await;
        sqlx::query(&format!("DROP DATABASE {db}"))
            .execute(&mut admin)
            .await
            .unwrap();
    }
}
