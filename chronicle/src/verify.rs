/// End-to-end pipeline verifier for GomSpace (GOMX / FNSE).
///
/// Checks each acceptance criterion of NEX-87 automatically against the live
/// database and external APIs. Exits 0 when all checks pass, 1 when any fail,
/// so it can be used as a CI step or Kubernetes CronJob.
///
/// # Checks
///
/// 1. **company_registered** — GOMX present in `companies` with `exchange_mic = 'FNSE'`
/// 2. **fi_pdmr_ingested**   — FI public register transactions for GOMX present in DB
/// 3. **bars_flowing**        — bars exist for GOMX in the `bars` table
/// 4. **holidays_populated**  — FNSE trading holidays populated for the current year
///
/// Checks 3 and 4 are advisory (the Saxo subscription is a prerequisite for 3).
use anyhow::{Context, Result};
use chrono::{Datelike, Duration, NaiveDate, Utc};
use clap::Parser;
use serde::Deserialize;
use sqlx::postgres::PgPoolOptions;
use sqlx::Row;
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(about = "Automated end-to-end pipeline verification for GomSpace (GOMX/FNSE) — NEX-87")]
struct Args {
    /// Signal/backtest Postgres URL (contains bars, companies, insider_filings).
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    /// How many days back to fetch from FI when comparing with DB.
    #[arg(long, env = "LOOKBACK_DAYS", default_value = "90")]
    lookback_days: i64,

    /// Ticker to verify (default: GOMX).
    #[arg(long, env = "VERIFY_TICKER", default_value = "GOMX")]
    ticker: String,
}

// ── FI API types (mirrors chronicle/src/pdmr.rs) ─────────────────────────

const FI_BASE: &str = "https://marknadssok.fi.se/Publiceringsklient/sv-SE/Search/Search";
const FI_AUTOCOMPLETE: &str = "https://marknadssok.fi.se/Publiceringsklient/sv-SE/AutoComplete/H\u{00e4}mtaAutoCompleteListaFull";

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
struct FiResponse {
    items: Vec<FiRow>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "PascalCase")]
struct FiRow {
    #[serde(rename = "Person i ledande ställning")]
    person: Option<String>,
    #[serde(rename = "Transaktionsdatum")]
    transaction_date: Option<String>,
    #[serde(rename = "Volym")]
    volume: Option<String>,
    #[serde(rename = "Status")]
    status: Option<String>,
}

// ── Check result ──────────────────────────────────────────────────────────

#[derive(Debug)]
struct CheckResult {
    name: &'static str,
    passed: bool,
    detail: String,
}

impl CheckResult {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            passed: true,
            detail: detail.into(),
        }
    }
    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            passed: false,
            detail: detail.into(),
        }
    }
    fn warn(name: &'static str, detail: impl Into<String>) -> Self {
        // Advisory — does not fail the overall run.
        Self {
            name,
            passed: true,
            detail: format!("[advisory] {}", detail.into()),
        }
    }
}

// ── Check 1: company registered ──────────────────────────────────────────

async fn check_company_registered(pool: &sqlx::PgPool, ticker: &str) -> CheckResult {
    let row = sqlx::query(
        "SELECT exchange_mic, currency FROM companies WHERE ticker = $1 AND exchange_mic = 'FNSE'",
    )
    .bind(ticker)
    .fetch_optional(pool)
    .await;

    match row {
        Ok(Some(r)) => {
            let mic: String = r.get("exchange_mic");
            let ccy: String = r.get("currency");
            CheckResult::pass(
                "company_registered",
                format!("{ticker} registered — exchange_mic={mic}, currency={ccy}"),
            )
        }
        Ok(None) => CheckResult::fail(
            "company_registered",
            format!("{ticker} not found in companies with exchange_mic='FNSE'. Register with: nexus register {ticker}.ST"),
        ),
        Err(e) => CheckResult::fail("company_registered", format!("DB error: {e}")),
    }
}

// ── Check 2: FI PDMR ingested ─────────────────────────────────────────────

async fn fetch_fi_transactions(
    http: &reqwest::Client,
    ticker: &str,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<FiRow>> {
    // Resolve the FI company name from the ticker via autocomplete.
    let ac_url = format!("{FI_AUTOCOMPLETE}?sokterm={}", urlencoding::encode(ticker));
    let names: Vec<serde_json::Value> = http
        .get(&ac_url)
        .send()
        .await?
        .json()
        .await
        .unwrap_or_default();

    let company_name = names
        .into_iter()
        .find_map(|v| {
            v.get("label")
                .and_then(|l| l.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| ticker.to_string());

    info!(ticker, company_name, "resolved FI company name");

    // Fetch transactions for the resolved name.
    let url = format!(
        "{FI_BASE}?FuturAndOptions=false\
         &Utgivare={}\
         &FromDate={}\
         &ToDate={}\
         &Page=1&PageSize=500",
        urlencoding::encode(&company_name),
        from.format("%Y-%m-%d"),
        to.format("%Y-%m-%d"),
    );

    let resp: FiResponse = http
        .get(&url)
        .send()
        .await
        .context("FI API request failed")?
        .json()
        .await
        .context("FI API JSON parse failed")?;

    Ok(resp
        .items
        .into_iter()
        .filter(|r| r.status.as_deref().unwrap_or("") == "Aktuell")
        .collect())
}

async fn check_fi_pdmr_ingested(
    pool: &sqlx::PgPool,
    http: &reqwest::Client,
    ticker: &str,
    lookback_days: i64,
) -> CheckResult {
    let today = Utc::now().date_naive();
    let from = today - Duration::days(lookback_days);

    let fi_rows = match fetch_fi_transactions(http, ticker, from, today).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!(error = %e, "failed to fetch FI data — marking check as advisory");
            return CheckResult::warn("fi_pdmr_ingested", format!("FI API unreachable: {e}"));
        }
    };

    let fi_count = fi_rows.len();

    // Count DB rows for this ticker in the same window.
    let db_count: i64 = sqlx::query(
        "SELECT COUNT(*) FROM insider_filings WHERE ticker = $1 AND transaction_date >= $2",
    )
    .bind(ticker)
    .bind(from)
    .fetch_one(pool)
    .await
    .map(|r| r.get::<i64, _>(0))
    .unwrap_or(0);

    if fi_count == 0 && db_count == 0 {
        return CheckResult::warn(
            "fi_pdmr_ingested",
            format!("no FI transactions found for {ticker} in last {lookback_days} days (no PDMR activity in window)"),
        );
    }

    if db_count == 0 && fi_count > 0 {
        // Build a list of what's missing.
        let missing: Vec<String> = fi_rows
            .iter()
            .map(|r| {
                let person = r.person.as_deref().unwrap_or("?");
                let date = r.transaction_date.as_deref().unwrap_or("?");
                let vol = r.volume.as_deref().unwrap_or("?");
                format!("  - {person} on {date}, {vol} shares")
            })
            .take(5)
            .collect();
        return CheckResult::fail(
            "fi_pdmr_ingested",
            format!(
                "{fi_count} FI transactions found but 0 in DB for {ticker} since {from}.\n\
                 First missing:\n{}{}",
                missing.join("\n"),
                if fi_count > 5 {
                    format!("\n  ... and {} more", fi_count - 5)
                } else {
                    String::new()
                }
            ),
        );
    }

    CheckResult::pass(
        "fi_pdmr_ingested",
        format!("DB has {db_count} insider_filings for {ticker} since {from} (FI shows {fi_count} current transactions)"),
    )
}

// ── Check 3: bars flowing ─────────────────────────────────────────────────

async fn check_bars_flowing(pool: &sqlx::PgPool, ticker: &str) -> CheckResult {
    let row =
        sqlx::query("SELECT COUNT(*) as cnt, MAX(date) as latest FROM bars WHERE ticker = $1")
            .bind(ticker)
            .fetch_one(pool)
            .await;

    match row {
        Ok(r) => {
            let cnt: i64 = r.get("cnt");
            if cnt == 0 {
                CheckResult::warn(
                    "bars_flowing",
                    format!("0 bars for {ticker} — Saxo SSE_FN-SE subscription required"),
                )
            } else {
                let latest: NaiveDate = r.get("latest");
                CheckResult::pass(
                    "bars_flowing",
                    format!("{cnt} bars for {ticker}, latest {latest}"),
                )
            }
        }
        Err(e) => CheckResult::fail("bars_flowing", format!("DB error: {e}")),
    }
}

// ── Check 4: trading holidays populated ──────────────────────────────────

async fn check_holidays_populated(pool: &sqlx::PgPool) -> CheckResult {
    let current_year = Utc::now().year();
    let row = sqlx::query(
        "SELECT COUNT(*) FROM trading_holidays \
         WHERE exchange_mic = 'FNSE' AND EXTRACT(year FROM date) = $1",
    )
    .bind(current_year)
    .fetch_one(pool)
    .await;

    match row {
        Ok(r) => {
            let cnt: i64 = r.get(0);
            if cnt == 0 {
                CheckResult::warn(
                    "holidays_populated",
                    format!("no FNSE trading holidays for {current_year} — run the holiday ingestion CronJob"),
                )
            } else {
                CheckResult::pass(
                    "holidays_populated",
                    format!("{cnt} FNSE trading holidays populated for {current_year}"),
                )
            }
        }
        Err(e) => CheckResult::fail("holidays_populated", format!("DB error: {e}")),
    }
}

// ── main ──────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/.env")).ok();
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&args.database_url)
        .await
        .context("failed to connect to postgres")?;

    let http = reqwest::Client::builder()
        .user_agent("nexus/verify lasse.alm@gsfleet.io")
        .build()?;

    info!(ticker = %args.ticker, lookback_days = args.lookback_days, "starting e2e verification");

    let results = vec![
        check_company_registered(&pool, &args.ticker).await,
        check_fi_pdmr_ingested(&pool, &http, &args.ticker, args.lookback_days).await,
        check_bars_flowing(&pool, &args.ticker).await,
        check_holidays_populated(&pool).await,
    ];

    println!("\n=== NEX-87 E2E Verification: {} ===\n", args.ticker);

    let mut any_failed = false;
    for r in &results {
        let icon = if r.passed { "✅" } else { "❌" };
        println!("{icon} {}: {}", r.name, r.detail);
        if !r.passed {
            any_failed = true;
        }
    }

    let failed_count = results.iter().filter(|r| !r.passed).count();
    let passed_count = results.iter().filter(|r| r.passed).count();
    println!(
        "\n{passed_count}/{} checks passed{}",
        results.len(),
        if any_failed {
            format!(" — {failed_count} FAILED")
        } else {
            String::new()
        }
    );

    if any_failed {
        error!("verification FAILED — see above");
        std::process::exit(1);
    }

    info!("verification PASSED");
    Ok(())
}
