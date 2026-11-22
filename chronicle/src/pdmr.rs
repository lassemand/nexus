/// FI (Finansinspektionen) PDMR insider-transaction ingestion binary.
///
/// Fetches MAR Article 19 PDMR disclosures from Sweden's financial regulator
/// and publishes them as `UnifiedInsiderTransaction` (NEX-92) to Kafka.
///
/// Mirrors `chronicle/src/filings.rs` structurally (SEC Form 4 path) — both
/// publish to the shared `insider.transactions` topic using the unified schema.
///
/// Companies are read from the `companies` Postgres table for every MIC of a
/// country whose insider source is the FI register
/// ([`model::nordic::mics_with_insider_source`]; today `FNSE` and `XSTO`) — no
/// hardcoded tickers or MICs. Each message carries the company's own MIC. A
/// company with no `fi_issuer` whose ticker FI's autocomplete does not know is
/// skipped with a warning and counted in `pdmr_companies_without_issuer`. The table is first synced
/// from `company.tickers` ([`chronicle_lib::registry`]), and a company's
/// `fi_issuer` column, when set, is the FI issuer name searched for.
///
/// # Data boundary
///
/// Pre-July-2016 (pre-MAR) FI data unavailable; `from` clamped to 2016-07-03.
///
/// # Correction handling
///
/// FI corrections: `Status != "Aktuell"` rows are filtered before publishing.
/// Corrections are published with `is_amendment = true` and a stable
/// `amended_transaction_id` (via `model::insider::transaction_id`) so
/// log-compacted consumers overwrite the original.
mod db;
mod kafka;

use anyhow::Context;
use chronicle_lib::insider_source::{
    fi_register::{FiEndpoints, MAR_START},
    FiRegister, SourceContext,
};
use chronicle_lib::nordic::db::Company;
use chronicle_lib::registry;
use chrono::{Duration, NaiveDate, Utc};
use clap::Parser;
use kafka::ChronicleProducer;
use model::generated::UnifiedInsiderTransaction;
use model::nordic::{self, InsiderSourceKind};
use sqlx::postgres::PgPoolOptions;
use sqlx::Row;
use std::process::ExitCode;
use tracing::{error, info, warn};

// ── Args ──────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(about = "Fetch FI PDMR insider transactions and publish as InsiderTransaction to Kafka")]
struct Args {
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    #[arg(long, env = "KAFKA_BROKERS")]
    kafka_brokers: String,

    // Not `KAFKA_TOPIC`: chronicle-secret sets that to `earnings.calendar` for
    // every chronicle job. Same variable signal consumes with.
    #[arg(
        long,
        env = "INSIDER_TRANSACTIONS_TOPIC",
        default_value = "insider.transactions"
    )]
    kafka_topic: String,

    #[arg(long, env = "KAFKA_TICKERS_TOPIC", default_value = registry::TICKERS_TOPIC)]
    tickers_topic: String,

    #[arg(long, env = "LOOKBACK_DAYS", default_value = "90")]
    lookback_days: i64,
}

// ── Registry query ────────────────────────────────────────────────────────

/// Load every registered company on a venue whose country reads the FI
/// register, with its FI issuer name when mapped.
async fn load_fi_companies(
    pool: &sqlx::PgPool,
    mics: &[&str],
) -> anyhow::Result<Vec<(Company, Option<String>)>> {
    let mics: Vec<String> = mics.iter().map(|m| m.to_string()).collect();
    let rows = sqlx::query(
        "SELECT ticker, exchange_mic, fi_issuer FROM companies \
         WHERE exchange_mic = ANY($1) ORDER BY exchange_mic, ticker",
    )
    .bind(&mics)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            (
                Company {
                    ticker: r.get("ticker"),
                    exchange_mic: r.get("exchange_mic"),
                    mfn_slug: None,
                    name: None,
                    isin: None,
                },
                r.get("fi_issuer"),
            )
        })
        .collect())
}

/// What a pass over the companies produced.
struct Collected {
    /// Transactions per company, in the order fetched.
    transactions: Vec<(Company, Vec<UnifiedInsiderTransaction>)>,
    /// Companies with no `fi_issuer` for which FI's ticker search found nothing.
    without_issuer: u64,
}

/// Fetch every company's transactions from FI. A company without an issuer is
/// counted and skipped; a failed fetch is logged and skipped.
async fn collect(
    http: &reqwest::Client,
    endpoints: &FiEndpoints,
    companies: &[(Company, Option<String>)],
    from: NaiveDate,
    pause: std::time::Duration,
) -> Collected {
    let mut out = Collected {
        transactions: Vec::new(),
        without_issuer: 0,
    };
    for (i, (company, fi_issuer)) in companies.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(pause).await;
        }
        let ticker = &company.ticker;
        let mic = &company.exchange_mic;
        let mut ctx = SourceContext {
            http,
            feed: None,
            fi_issuer: fi_issuer.as_deref(),
        };
        let issuer = match FiRegister::issuer(&ctx, endpoints, company).await {
            Ok(Some(issuer)) => issuer,
            Ok(None) => {
                warn!(
                    ticker,
                    mic,
                    "no FI issuer for company, skipping — set companies.fi_issuer (nexus register)"
                );
                out.without_issuer += 1;
                continue;
            }
            Err(e) => {
                error!(ticker, mic, error = %e, "failed to resolve FI issuer");
                continue;
            }
        };
        ctx.fi_issuer = Some(&issuer);

        info!(ticker, mic, from = %from, "fetching insider transactions");
        match FiRegister::fetch_from(&mut ctx, endpoints, company, from).await {
            Ok(t) => {
                info!(ticker, mic, count = t.len(), "fetched PDMR transactions");
                metrics::counter!("pdmr_rows_fetched_total").increment(t.len() as u64);
                out.transactions.push((company.clone(), t));
            }
            Err(e) => error!(ticker, mic, error = %e, "failed to fetch insider transactions"),
        }
    }
    out
}

// ── Main ──────────────────────────────────────────────────────────────────

/// Job name in the Pushgateway. Run metrics: `pdmr_rows_fetched_total` (FI register
/// rows, one transaction each), `pdmr_transactions_published_total`,
/// `pdmr_parse_errors_total` (rows skipped for an unreadable date), gauge
/// `pdmr_companies_without_issuer`.
const JOB: &str = "pdmr";

#[tokio::main]
async fn main() -> ExitCode {
    dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/.env")).ok();
    tracing_subscriber::fmt::init();
    match run_job(Args::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!(error = %format!("{e:#}"), "pdmr failed");
            ExitCode::FAILURE
        }
    }
}

/// Runs the job and pushes its metrics, on success and on error alike.
async fn run_job(args: Args) -> anyhow::Result<()> {
    telemetry::run_batch(JOB, run(args)).await
}

async fn run(args: Args) -> anyhow::Result<()> {
    for name in [
        "pdmr_rows_fetched_total",
        "pdmr_transactions_published_total",
        "pdmr_parse_errors_total",
    ] {
        metrics::counter!(name).absolute(0);
    }
    metrics::gauge!("pdmr_companies_without_issuer").set(0.0);
    // Connect to DB and run migrations so the companies table is up-to-date.
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&args.database_url)
        .await
        .context("failed to connect to postgres")?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("migrations failed")?;

    registry::sync_or_warn(&pool, Some(&args.kafka_brokers), &args.tickers_topic).await;

    let mics = nordic::mics_with_insider_source(InsiderSourceKind::FiRegister);
    let companies = match load_fi_companies(&pool, &mics).await {
        Ok(t) => t,
        Err(e) => {
            error!(error = %e, "failed to load FI-register companies from companies table");
            return Err(e.context("failed to load FI-register companies from companies table"));
        }
    };

    if companies.is_empty() {
        warn!(
            "no FI-register companies registered — register tickers via: \
             nexus register <TICKER> --exchange-mic <{}>",
            mics.join("|")
        );
        return Ok(());
    }

    info!(
        count = companies.len(),
        mics = %mics.join(","),
        "loaded FI-register companies from companies table"
    );

    let client = reqwest::Client::builder()
        .user_agent("nexus lasse.alm@gsfleet.io")
        .build()
        .context("failed to build HTTP client")?;

    let producer =
        ChronicleProducer::new(&args.kafka_brokers).context("failed to connect to Kafka")?;

    let to = Utc::now().date_naive();
    let mar_start = NaiveDate::parse_from_str(MAR_START, "%Y-%m-%d")?;
    // Pre-MAR data is unavailable — clamp from to 2016-07-03.
    let from = (to - Duration::days(args.lookback_days)).max(mar_start);

    let collected = collect(
        &client,
        &FiEndpoints::live(),
        &companies,
        from,
        std::time::Duration::from_millis(500),
    )
    .await;
    metrics::gauge!("pdmr_companies_without_issuer").set(collected.without_issuer as f64);

    for (company, transactions) in collected.transactions {
        let ticker = &company.ticker;
        for txn in transactions {
            if let Err(e) = producer
                .publish_unified_insider_transaction(&args.kafka_topic, txn.clone())
                .await
            {
                error!(ticker, error = %e, "failed to publish UnifiedInsiderTransaction");
            } else {
                metrics::counter!("pdmr_transactions_published_total").increment(1);
                info!(
                    ticker,
                    mic = %txn.exchange_mic,
                    pdmr = %txn.person_name,
                    txn_type = txn.transaction_type,
                    volume = txn.volume,
                    price = txn.price_per_unit,
                    currency = %txn.currency,
                    amendment = txn.is_amendment,
                    "published UnifiedInsiderTransaction (FI)"
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{any, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn company(ticker: &str, mic: &str) -> Company {
        Company {
            ticker: ticker.to_string(),
            exchange_mic: mic.to_string(),
            mfn_slug: None,
            name: None,
            isin: None,
        }
    }

    fn endpoints(server: &MockServer) -> FiEndpoints {
        FiEndpoints {
            search: format!("{}/search", server.uri()),
            autocomplete: format!("{}/autocomplete", server.uri()),
        }
    }

    fn utf16(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
    }

    fn csv(person: &str) -> Vec<u8> {
        utf16(&format!(
            "Publiceringsdatum;Utgivare;LEI-kod;Anmälningsskyldig;Person i ledande ställning;Befattning;Närstående;Korrigering;Beskrivning av korrigering;Är undantag;Undantag;Karaktär;Instrumenttyp;Instrumentnamn;ISIN;Transaktionsdatum;Volym;Volymsenhet;Pris;Valuta;Handelsplats;Status\n\
             2026-11-10 09:00:00;X;LEI;{person};;VD;;;;;;Förvärv;Aktie;X;SE0000000001;2026-11-09 00:00:00;100;Antal;10,5;SEK;XSTO;Aktuell\n"
        ))
    }

    async fn mount_issuer_csv(server: &MockServer, issuer: &str, person: &str) {
        Mock::given(method("GET"))
            .and(path("/search"))
            .and(query_param("Utgivare", issuer))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(csv(person)))
            .mount(server)
            .await;
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn loads_every_fi_register_company_and_no_other(pool: sqlx::PgPool) {
        for (ticker, mic) in [
            ("BOL", "XSTO"),
            ("GOMX", "FNSE"),
            ("NOVO", "XCSE"),
            ("NOKIA", "XHEL"),
        ] {
            sqlx::query(
                "INSERT INTO companies (ticker, exchange_mic, currency) VALUES ($1, $2, 'SEK')",
            )
            .bind(ticker)
            .bind(mic)
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query("UPDATE companies SET fi_issuer = 'Boliden AB' WHERE ticker = 'BOL'")
            .execute(&pool)
            .await
            .unwrap();

        let mics = nordic::mics_with_insider_source(InsiderSourceKind::FiRegister);
        let loaded = load_fi_companies(&pool, &mics).await.unwrap();
        let got: Vec<(String, String, Option<String>)> = loaded
            .into_iter()
            .map(|(c, i)| (c.ticker, c.exchange_mic, i))
            .collect();
        assert_eq!(
            got,
            vec![
                ("GOMX".to_string(), "FNSE".to_string(), None),
                (
                    "BOL".to_string(),
                    "XSTO".to_string(),
                    Some("Boliden AB".to_string())
                ),
            ]
        );
    }

    #[tokio::test]
    async fn xsto_and_fnse_companies_are_fetched_with_their_own_mic() {
        let server = MockServer::start().await;
        mount_issuer_csv(&server, "Boliden AB", "Jane Doe").await;
        mount_issuer_csv(&server, "Gomspace Group AB", "John Roe").await;
        let companies = vec![
            (company("BOL", "XSTO"), Some("Boliden AB".to_string())),
            (
                company("GOMX", "FNSE"),
                Some("Gomspace Group AB".to_string()),
            ),
        ];
        let from = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();

        let collected = collect(
            &reqwest::Client::new(),
            &endpoints(&server),
            &companies,
            from,
            std::time::Duration::ZERO,
        )
        .await;

        assert_eq!(collected.without_issuer, 0);
        let got: Vec<(String, String, String)> = collected
            .transactions
            .iter()
            .flat_map(|(c, t)| {
                t.iter().map(|x| {
                    (
                        c.ticker.clone(),
                        x.exchange_mic.clone(),
                        x.person_name.clone(),
                    )
                })
            })
            .collect();
        assert_eq!(
            got,
            vec![
                (
                    "BOL".to_string(),
                    "XSTO".to_string(),
                    "Jane Doe".to_string()
                ),
                (
                    "GOMX".to_string(),
                    "FNSE".to_string(),
                    "John Roe".to_string()
                ),
            ]
        );
    }

    #[tokio::test]
    async fn a_company_without_issuer_is_counted_and_the_run_continues() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/autocomplete"))
            .and(query_param("sokterm", "ABB"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<String>::new()))
            .mount(&server)
            .await;
        mount_issuer_csv(&server, "Boliden AB", "Jane Doe").await;
        let companies = vec![
            (company("ABB", "XSTO"), None),
            (company("BOL", "XSTO"), Some("Boliden AB".to_string())),
        ];
        let from = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();

        let collected = collect(
            &reqwest::Client::new(),
            &endpoints(&server),
            &companies,
            from,
            std::time::Duration::ZERO,
        )
        .await;

        assert_eq!(collected.without_issuer, 1);
        assert_eq!(collected.transactions.len(), 1);
        assert_eq!(collected.transactions[0].0.ticker, "BOL");
    }

    /// A forced error (unusable database URL) still pushes the run, as a failure.
    #[tokio::test]
    async fn a_failed_run_still_pushes_its_metrics() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        std::env::set_var("PUSHGATEWAY_URL", server.uri());
        let args = Args::parse_from([
            "pdmr",
            "--database-url",
            "not-a-url",
            "--kafka-brokers",
            "none:1",
        ]);
        assert!(run_job(args).await.is_err());
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/metrics/job/pdmr");
        let body = String::from_utf8(requests[0].body.clone()).unwrap();
        assert!(body.contains("nexus_job_last_run_success{job=\"pdmr\"} 0"));
    }
}
