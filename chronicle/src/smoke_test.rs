use super::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A weekday date within `year` — `filter_holidays` (alpha::calendar)
/// drops anything landing on a Saturday/Sunday, so a hardcoded date
/// like Dec 25 would be flaky depending on which year this runs in.
fn a_weekday_in(year: i32) -> NaiveDate {
    let mut d = NaiveDate::from_ymd_opt(year, 6, 15).expect("valid date");
    while d.weekday() == chrono::Weekday::Sat || d.weekday() == chrono::Weekday::Sun {
        d += Duration::days(1);
    }
    d
}

/// Full, self-contained smoke test for the PR-gating check — no live
/// cluster, no real external APIs. Needs TEST_DATABASE_URL pointed at a
/// throwaway Postgres (the GH Actions services container in CI); skips
/// gracefully if unset so `cargo test` still works for a contributor
/// without a local Postgres running.
#[tokio::test]
async fn all_checks_pass_against_seeded_fixtures_and_mocked_apis() {
    let Ok(database_url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!(
            "TEST_DATABASE_URL not set — skipping smoke test (see .github/workflows/verify.yml)"
        );
        return;
    };

    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("failed to connect to test postgres");

    sqlx::migrate!("../signal/migrations")
        .run(&pool)
        .await
        .expect("failed to run signal/migrations against test postgres");

    const TICKER: &str = "SMOKETEST";
    let today = Utc::now().date_naive();
    let current_year = Utc::now().year();
    let holiday_date = a_weekday_in(current_year);

    // ── Seed fixtures ────────────────────────────────────────────────
    sqlx::query(
        "INSERT INTO companies (ticker, sector, exchange_mic, currency)
         VALUES ($1, 'Technology', 'FNSE', 'SEK')
         ON CONFLICT (ticker, exchange_mic) DO NOTHING",
    )
    .bind(TICKER)
    .execute(&pool)
    .await
    .expect("failed to seed companies");

    let person_id: i64 =
        sqlx::query("INSERT INTO persons (filer_name) VALUES ('Smoke Test Person') RETURNING id")
            .fetch_one(&pool)
            .await
            .expect("failed to seed persons")
            .get(0);

    sqlx::query(
        "INSERT INTO insider_filings
            (person_id, ticker, issuer_cik, filer_role, transaction_date,
             filing_date, shares, price_per_share, transaction_code)
         VALUES ($1, $2, 'SMOKE-CIK', 'Director', $3, $3, 100, 10.5, 'A')
         ON CONFLICT DO NOTHING",
    )
    .bind(person_id)
    .bind(TICKER)
    .bind(today)
    .execute(&pool)
    .await
    .expect("failed to seed insider_filings");

    sqlx::query(
        "INSERT INTO bars (ticker, date, open, high, low, close, volume)
         VALUES ($1, $2, 10.0, 11.0, 9.5, 10.5, 1000)
         ON CONFLICT (ticker, date) DO NOTHING",
    )
    .bind(TICKER)
    .bind(today)
    .execute(&pool)
    .await
    .expect("failed to seed bars");

    // ── Mock external APIs ───────────────────────────────────────────
    let fi_mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/Publiceringsklient/sv-SE/AutoComplete/H\u{00e4}mtaAutoCompleteListaFull",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!([{ "label": "Smoke Test AB" }])),
        )
        .mount(&fi_mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/Publiceringsklient/sv-SE/Search/Search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "items": [{
                "Person i ledande ställning": "Smoke Test Person",
                "Transaktionsdatum": today.format("%Y-%m-%d").to_string(),
                "Volym": "100",
                "Status": "Aktuell"
            }]
        })))
        .mount(&fi_mock)
        .await;

    let nager_mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v3/publicholidays/{current_year}/SE")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                "date": holiday_date.format("%Y-%m-%d").to_string(),
                "name": "Smoke Test Holiday"
            }])),
        )
        .mount(&nager_mock)
        .await;

    // ── Run the actual checks against seeded data + mocked APIs ──────
    let http = reqwest::Client::new();
    let calendar_provider = alpha::calendar::CalendarProvider::with_base_url(nager_mock.uri());

    bootstrap_holidays(&pool, &calendar_provider)
        .await
        .expect("bootstrap_holidays failed against mock Nager.Date");

    let results = vec![
        check_company_registered(&pool, TICKER).await,
        check_fi_pdmr_ingested(&pool, &http, &fi_mock.uri(), TICKER, 90).await,
        check_bars_flowing(&pool, TICKER).await,
        check_holidays_populated(&pool).await,
    ];

    for r in &results {
        assert!(
            r.passed,
            "check {} unexpectedly failed against seeded fixtures: {}",
            r.name, r.detail
        );
    }
}
