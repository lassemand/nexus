/// FI (Finansinspektionen) PDMR insider-transaction ingestion binary.
///
/// Fetches MAR Article 19 PDMR disclosures from Sweden's financial regulator
/// and publishes them as `InsiderTransaction` protobuf messages to Kafka.
///
/// # Design
///
/// Mirrors `chronicle/src/filings.rs` structurally (SEC Form 4 path) but is
/// a fully separate ingestion path — no EDGAR/SEC code is touched.
///
/// Tickers are read from the `companies` Postgres table (extended in NEX-80)
/// filtered by `exchange_mic = 'FNSE'`. This means any Nordic ticker
/// registered via `nexus register <TICKER> --exchange-mic FNSE` is picked up
/// automatically without hardcoding.
///
/// # Data source
///
/// FI's public MAR register (`marknadssok.fi.se`) — no authentication.
/// Protocol documented in `docs/adr/0002-fi-pdmr-register-access.md`.
///
/// # Data boundary
///
/// Pre-July-2016 (pre-MAR) FI data uses a different regulatory regime and is
/// not available via this endpoint. `LOOKBACK_DAYS` is clamped so that
/// `from` never goes earlier than 2016-07-03.
///
/// # Correction handling
///
/// FI marks corrections with `Korrigering` and a description. We:
/// 1. Filter out `Status != "Aktuell"` (already-superseded rows).
/// 2. Publish corrections with the same Kafka message key as the original
///    (`{ticker}:{pdmr_name}:{transaction_date}:{isin}`) so log-compacted
///    consumers overwrite rather than accumulate duplicate records.
///    The `is_correction` / `correction_description` fields in `InsiderTransaction`
///    let consumers distinguish original from amendment.
///
/// # Kafka topic
///
/// Default: `insider.transactions.nordic` — distinct from `insider.filings`
/// (SEC Form 4) to avoid mixing SEC and MAR-regime records.
mod db;
mod kafka;

use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use clap::Parser;
use kafka::ChronicleProducer;
use model::generated::{InsiderTransaction, TransactionType};
use sqlx::postgres::PgPoolOptions;
use sqlx::Row;
use tracing::{error, info, warn};

const FI_BASE: &str = "https://marknadssok.fi.se/Publiceringsklient/sv-SE/Search/Search";
const FI_AUTOCOMPLETE: &str =
    "https://marknadssok.fi.se/Publiceringsklient/sv-SE/AutoComplete/H\u{00e4}mtaAutoCompleteListaFull";

/// Pre-MAR data boundary. FI's MAR register reliably covers from this date.
const MAR_START: &str = "2016-07-03";

// ── Args ──────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(about = "Fetch FI PDMR insider transactions and publish as InsiderTransaction to Kafka")]
struct Args {
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    #[arg(long, env = "KAFKA_BROKERS")]
    kafka_brokers: String,

    #[arg(
        long,
        env = "KAFKA_TOPIC",
        default_value = "insider.transactions.nordic"
    )]
    kafka_topic: String,

    #[arg(long, env = "LOOKBACK_DAYS", default_value = "90")]
    lookback_days: i64,
}

// ── Registry query ────────────────────────────────────────────────────────

/// Load all FNSE-tagged tickers from the `companies` table.
async fn load_fnse_tickers(pool: &sqlx::PgPool) -> anyhow::Result<Vec<String>> {
    let rows =
        sqlx::query("SELECT ticker FROM companies WHERE exchange_mic = 'FNSE' ORDER BY ticker")
            .fetch_all(pool)
            .await?;

    Ok(rows
        .into_iter()
        .map(|r| r.get::<String, _>("ticker"))
        .collect())
}

// ── FI API ────────────────────────────────────────────────────────────────

/// Resolve a ticker to the exact FI issuer name via the autocomplete endpoint.
async fn resolve_issuer(client: &reqwest::Client, ticker: &str) -> anyhow::Result<Option<String>> {
    let url = format!(
        "{}?sokfunktion=Insyn&falt=Utgivare&sokterm={}",
        FI_AUTOCOMPLETE,
        urlencoding::encode(ticker)
    );
    let names: Vec<String> = client
        .get(&url)
        .header("User-Agent", "nexus lasse.alm@gsfleet.io")
        .send()
        .await?
        .json()
        .await?;
    Ok(names.into_iter().next())
}

// ── CSV parsing ───────────────────────────────────────────────────────────

/// A parsed row from FI's UTF-16 LE semicolon-delimited CSV export.
struct FiRow {
    publication_date: NaiveDateTime,
    /// Legal Entity Identifier — stored in InsiderTransaction.source_registry context.
    /// Not directly exposed in the proto but logged for traceability.
    #[allow(dead_code)]
    lei: String,
    reporting_person: String,
    role: String,
    is_close_associate: bool,
    is_correction: bool,
    correction_description: String,
    transaction_type: String, // "Förvärv", "Avyttring", etc.
    instrument_type: String,
    isin: String,
    transaction_date: NaiveDateTime,
    volume: f64,
    price: f64,
    currency: String,
}

fn parse_swedish_f64(s: &str) -> f64 {
    s.trim().replace(',', ".").parse().unwrap_or(0.0)
}

fn parse_fi_datetime(s: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S").ok()
}

fn map_transaction_type(karaktar: &str) -> i32 {
    if karaktar.contains("Förvärv") {
        TransactionType::Buy as i32
    } else if karaktar.contains("Avyttring") {
        TransactionType::Sell as i32
    } else {
        TransactionType::Other as i32
    }
}

/// Fetch and parse the PDMR CSV for a given FI issuer name and date range.
async fn fetch_pdmr_rows(
    client: &reqwest::Client,
    issuer: &str,
    from: NaiveDate,
    to: NaiveDate,
) -> anyhow::Result<Vec<FiRow>> {
    let url = format!(
        "{}?SearchFunctionType=Insyn&Utgivare={}&PersonILedandeStällningNamn=\
         &Transaktionsdatum.From={}&Transaktionsdatum.To={}\
         &Publiceringsdatum.From=&Publiceringsdatum.To=&button=export",
        FI_BASE,
        urlencoding::encode(issuer),
        from.format("%Y-%m-%d"),
        to.format("%Y-%m-%d"),
    );

    let bytes = client
        .get(&url)
        .header("User-Agent", "nexus lasse.alm@gsfleet.io")
        .send()
        .await?
        .bytes()
        .await?;

    // Response is UTF-16 LE.
    let text = String::from_utf16_lossy(
        &bytes
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect::<Vec<_>>(),
    );

    let mut rows = Vec::new();
    let mut lines = text.lines();

    // Header row: build column index map.
    let Some(header) = lines.next() else {
        return Ok(rows);
    };
    let cols: Vec<&str> = header.split(';').collect();
    let idx = |name: &str| cols.iter().position(|&c| c.trim() == name);

    let i_pub = idx("Publiceringsdatum").unwrap_or(0);
    let i_lei = idx("LEI-kod").unwrap_or(2);
    let i_rep = idx("Anmälningsskyldig").unwrap_or(3);
    let i_role = idx("Befattning").unwrap_or(5);
    let i_near = idx("Närstående").unwrap_or(6);
    let i_corr = idx("Korrigering").unwrap_or(7);
    let i_corr_desc = idx("Beskrivning av korrigering").unwrap_or(8);
    let i_char = idx("Karaktär").unwrap_or(11);
    let i_itype = idx("Instrumenttyp").unwrap_or(12);
    let i_isin = idx("ISIN").unwrap_or(14);
    let i_date = idx("Transaktionsdatum").unwrap_or(15);
    let i_vol = idx("Volym").unwrap_or(16);
    let i_price = idx("Pris").unwrap_or(18);
    let i_cur = idx("Valuta").unwrap_or(19);
    let i_status = idx("Status").unwrap_or(21);

    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(';').collect();
        let get = |i: usize| fields.get(i).copied().unwrap_or("").trim();

        // Only include active (non-superseded) rows.
        if get(i_status) != "Aktuell" {
            continue;
        }

        let Some(pub_dt) = parse_fi_datetime(get(i_pub)) else {
            continue;
        };
        let Some(txn_dt) = parse_fi_datetime(get(i_date)) else {
            continue;
        };

        rows.push(FiRow {
            publication_date: pub_dt,
            lei: get(i_lei).to_string(),
            reporting_person: get(i_rep).to_string(),
            role: get(i_role).to_string(),
            is_close_associate: !get(i_near).is_empty(),
            is_correction: !get(i_corr).is_empty(),
            correction_description: get(i_corr_desc).to_string(),
            transaction_type: get(i_char).to_string(),
            instrument_type: get(i_itype).to_string(),
            isin: get(i_isin).to_string(),
            transaction_date: txn_dt,
            volume: parse_swedish_f64(get(i_vol)),
            price: parse_swedish_f64(get(i_price)),
            currency: get(i_cur).to_string(),
        });
    }

    Ok(rows)
}

// ── Main ──────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/.env")).ok();
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    // Connect to DB and run migrations so the companies table is up-to-date.
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&args.database_url)
        .await
        .expect("failed to connect to postgres");

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrations failed");

    // Load FNSE-tagged tickers from the extended companies registry.
    let tickers = match load_fnse_tickers(&pool).await {
        Ok(t) => t,
        Err(e) => {
            error!(error = %e, "failed to load FNSE tickers from companies table");
            return;
        }
    };

    if tickers.is_empty() {
        warn!(
            "no FNSE tickers registered — register tickers via: \
             nexus register <TICKER> --exchange-mic FNSE"
        );
        return;
    }

    info!(
        count = tickers.len(),
        "loaded FNSE tickers from companies table"
    );

    let client = reqwest::Client::builder()
        .user_agent("nexus lasse.alm@gsfleet.io")
        .build()
        .expect("failed to build HTTP client");

    let producer = ChronicleProducer::new(&args.kafka_brokers).expect("failed to connect to Kafka");

    let to = Utc::now().date_naive();
    let mar_start = NaiveDate::parse_from_str(MAR_START, "%Y-%m-%d").unwrap();
    // Pre-MAR data is unavailable — clamp from to 2016-07-03.
    let from = (to - Duration::days(args.lookback_days)).max(mar_start);

    for (i, ticker) in tickers.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }

        // Resolve ticker → FI issuer name.
        let issuer = match resolve_issuer(&client, ticker).await {
            Ok(Some(name)) => name,
            Ok(None) => {
                warn!(ticker, "no FI issuer found for ticker, skipping");
                continue;
            }
            Err(e) => {
                error!(ticker, error = %e, "FI autocomplete request failed");
                continue;
            }
        };

        info!(ticker, issuer, from = %from, to = %to, "fetching PDMR transactions from FI");

        let rows = match fetch_pdmr_rows(&client, &issuer, from, to).await {
            Ok(r) => r,
            Err(e) => {
                error!(ticker, issuer, error = %e, "failed to fetch PDMR data from FI");
                continue;
            }
        };

        info!(
            ticker,
            issuer,
            count = rows.len(),
            "fetched PDMR transactions"
        );

        for row in &rows {
            let txn_unix = NaiveDateTime::new(
                row.transaction_date.date(),
                NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
            )
            .and_utc()
            .timestamp();

            let pub_unix = NaiveDateTime::new(
                row.publication_date.date(),
                NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
            )
            .and_utc()
            .timestamp();

            let txn = InsiderTransaction {
                ticker: ticker.clone(),
                exchange_mic: "FNSE".to_string(),
                pdmr_name: row.reporting_person.clone(),
                pdmr_role: row.role.clone(),
                is_close_associate: row.is_close_associate,
                transaction_date_unix_secs: txn_unix,
                publication_date_unix_secs: pub_unix,
                transaction_type: map_transaction_type(&row.transaction_type),
                volume: row.volume,
                price_per_unit: row.price,
                currency: row.currency.clone(),
                isin: row.isin.clone(),
                instrument_type: row.instrument_type.clone(),
                is_correction: row.is_correction,
                correction_description: row.correction_description.clone(),
                source_registry: "FI".to_string(),
            };

            if let Err(e) = producer
                .publish_insider_transaction(&args.kafka_topic, &txn)
                .await
            {
                error!(ticker, error = %e, "failed to publish InsiderTransaction");
            } else {
                info!(
                    ticker,
                    pdmr = %row.reporting_person,
                    txn_type = %row.transaction_type,
                    volume = row.volume,
                    price = row.price,
                    currency = %row.currency,
                    correction = row.is_correction,
                    "published InsiderTransaction"
                );
            }
        }
    }
}
