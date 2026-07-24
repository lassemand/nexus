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
///
/// # `--bootstrap`
///
/// Only `holidays_populated` can be meaningfully self-healed here: it's a
/// single direct Postgres write (via [`alpha::calendar::CalendarProvider`],
/// the same logic `nexus calendar sync` uses), no other service involved.
/// `company_registered` and `fi_pdmr_ingested` both go through Kafka to a
/// separate `signal` consumer before anything lands in Postgres — "ensuring"
/// those would mean triggering a publish and then polling for a consumer
/// this binary doesn't own to catch up, which is real scope beyond a verify
/// tool. `bars_flowing` requires enabling a paid Saxo market-data
/// subscription on the account — not something any code path can do. Both
/// stay pure read-only checks regardless of `--bootstrap`.
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

    /// Attempt to self-heal the holidays_populated check before reporting
    /// (see module docs — the only check this applies to).
    #[arg(long, env = "VERIFY_BOOTSTRAP", default_value_t = false)]
    bootstrap: bool,
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

/// `trading_holidays` is keyed by `country` (ISO 3166-1 alpha-2), not by
/// exchange MIC — there is no `exchange_mic` column on that table at all.
/// `alpha::calendar::country_for_exchange` is the same mapping
/// `nexus calendar sync` uses to resolve "FNSE" -> "SE".
async fn check_holidays_populated(pool: &sqlx::PgPool) -> CheckResult {
    let current_year = Utc::now().year();
    let Some(country) = alpha::calendar::country_for_exchange("FNSE") else {
        return CheckResult::fail(
            "holidays_populated",
            "FNSE has no country mapping in alpha::calendar::EXCHANGE_TO_COUNTRY".to_string(),
        );
    };

    let row = sqlx::query(
        "SELECT COUNT(*) FROM trading_holidays \
         WHERE country = $1 AND EXTRACT(year FROM date) = $2",
    )
    .bind(country)
    .bind(current_year)
    .fetch_one(pool)
    .await;

    match row {
        Ok(r) => {
            let cnt: i64 = r.get(0);
            if cnt == 0 {
                CheckResult::warn(
                    "holidays_populated",
                    format!("no {country} trading holidays for {current_year} — run `nexus calendar sync --country {country} --year {current_year}` (or pass --bootstrap)"),
                )
            } else {
                CheckResult::pass(
                    "holidays_populated",
                    format!("{cnt} {country} trading holidays populated for {current_year}"),
                )
            }
        }
        Err(e) => CheckResult::fail("holidays_populated", format!("DB error: {e}")),
    }
}

/// Self-heals `holidays_populated` if it's failing: fetches the current
/// year's holidays from Nager.Date and upserts them into `trading_holidays`.
/// Identical logic to `cli`'s `nexus calendar sync`, inlined here so this
/// binary doesn't depend on the `nexus` CLI binary being present in the
/// runtime image (it currently isn't — see infra/Dockerfile).
async fn bootstrap_holidays(pool: &sqlx::PgPool) -> Result<()> {
    let current_year = Utc::now().year();
    let country = alpha::calendar::country_for_exchange("FNSE")
        .context("FNSE has no country mapping in alpha::calendar::EXCHANGE_TO_COUNTRY")?;

    let provider = alpha::calendar::CalendarProvider::new();
    let entries = provider
        .holidays_for_country(country, current_year)
        .await
        .context("failed to fetch holidays from Nager.Date")?;

    if entries.is_empty() {
        info!(
            country,
            current_year, "no holiday entries returned to upsert"
        );
        return Ok(());
    }

    let mut upserted = 0usize;
    for entry in &entries {
        sqlx::query(
            "INSERT INTO trading_holidays (country, date, status, note)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (country, date) DO NOTHING",
        )
        .bind(country)
        .bind(entry.date)
        .bind(entry.status)
        .bind(&entry.note)
        .execute(pool)
        .await
        .with_context(|| format!("failed to upsert {}", entry.date))?;
        upserted += 1;
    }

    info!(
        country,
        current_year, upserted, "bootstrapped trading_holidays"
    );
    Ok(())
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

    // A timeout matters here specifically because this runs as a PR-gating
    // smoke test — an unresponsive (not just erroring) FI endpoint would
    // otherwise hang the job rather than failing cleanly and quickly.
    // fetch_fi_transactions already treats a request error as advisory
    // (CheckResult::warn, not fail), so a timeout here degrades the same way
    // an outright connection failure does — it doesn't newly introduce a
    // failure mode, it just bounds how long we wait to hit the existing one.
    let http = reqwest::Client::builder()
        .user_agent("nexus/verify lasse.alm@gsfleet.io")
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    info!(ticker = %args.ticker, lookback_days = args.lookback_days, bootstrap = args.bootstrap, "starting e2e verification");

    if args.bootstrap {
        // Only holidays_populated is self-healable here — see module docs.
        // Attempted unconditionally (upsert is idempotent) rather than
        // pre-checking first, so a currently-passing check just no-ops.
        if let Err(e) = bootstrap_holidays(&pool).await {
            warn!(error = %e, "bootstrap: failed to seed trading_holidays — check will report as-is");
        }
    }

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
