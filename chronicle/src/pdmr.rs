/// FI (Finansinspektionen) PDMR insider-transaction ingestion binary.
///
/// Fetches MAR Article 19 PDMR disclosures from Sweden's financial regulator
/// and publishes them as `UnifiedInsiderTransaction` (NEX-92) to Kafka.
///
/// Mirrors `chronicle/src/filings.rs` structurally (SEC Form 4 path) — both
/// publish to the shared `insider.transactions` topic using the unified schema.
///
/// Tickers are read from the `companies` Postgres table filtered by
/// `exchange_mic = 'FNSE'` — no hardcoded tickers.
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

use chrono::{Duration, NaiveDate, NaiveDateTime, Utc};
use clap::Parser;
use kafka::ChronicleProducer;
use model::{
    generated::{
        unified_insider_transaction, FiDetail, SourceRegistry, UnifiedInsiderTransaction,
        UnifiedTransactionType,
    },
    insider::transaction_id,
};
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

    #[arg(long, env = "KAFKA_TOPIC", default_value = "insider.transactions")]
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

/// A parsed row from FI's UTF-16 LE semicolon-delimited CSV export, before
/// conversion into the wire-level `UnifiedInsiderTransaction`. Keeping CSV
/// parsing decoupled from the proto type means `fetch_pdmr_rows` doesn't need
/// to know about `SourceDetail`/`FiDetail` wrapping at all — mirrors the same
/// raw-struct pattern used for SEC (`SecFormFourTransaction` in `filings.rs`).
#[derive(Debug, Clone)]
struct FiRow {
    ticker: String,
    publication_date: NaiveDateTime,
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

impl From<FiRow> for UnifiedInsiderTransaction {
    fn from(r: FiRow) -> Self {
        let transaction_date = r.transaction_date.format("%Y-%m-%d").to_string();
        let published_date = r.publication_date.format("%Y-%m-%d").to_string();

        // Corrections share the same key as the original so log-compacted
        // consumers overwrite rather than accumulate duplicate records.
        let amended_transaction_id = if r.is_correction {
            transaction_id("FI", &r.ticker, &r.reporting_person, &transaction_date)
        } else {
            String::new()
        };

        UnifiedInsiderTransaction {
            ticker: r.ticker,
            exchange_mic: "FNSE".to_string(),
            source_registry: SourceRegistry::Fi as i32,
            person_name: r.reporting_person,
            person_role: r.role,
            transaction_date,
            published_date,
            transaction_type: map_transaction_type(&r.transaction_type),
            volume: r.volume,
            price_per_unit: r.price,
            currency: r.currency,
            is_amendment: r.is_correction,
            amended_transaction_id,
            source_detail: Some(unified_insider_transaction::SourceDetail::Fi(FiDetail {
                lei: r.lei,
                isin: r.isin,
                instrument_type: r.instrument_type,
                is_close_associate: r.is_close_associate,
                correction_description: r.correction_description,
            })),
        }
    }
}

fn parse_swedish_f64(s: &str) -> f64 {
    s.trim().replace(',', ".").parse().unwrap_or(0.0)
}

fn parse_fi_datetime(s: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S").ok()
}

fn map_transaction_type(karaktar: &str) -> i32 {
    if karaktar.contains("Förvärv") {
        UnifiedTransactionType::Buy as i32
    } else if karaktar.contains("Avyttring") {
        UnifiedTransactionType::Sell as i32
    } else {
        UnifiedTransactionType::Other as i32
    }
}

/// Fetch and parse the PDMR CSV for a given ticker/FI issuer name and date range.
async fn fetch_pdmr_rows(
    client: &reqwest::Client,
    ticker: &str,
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
            ticker: ticker.to_string(),
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

        let rows = match fetch_pdmr_rows(&client, ticker, &issuer, from, to).await {
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
            if let Err(e) = producer
                .publish_unified_insider_transaction(&args.kafka_topic, row.clone())
                .await
            {
                error!(ticker, error = %e, "failed to publish UnifiedInsiderTransaction");
            } else {
                info!(
                    ticker,
                    pdmr = %row.reporting_person,
                    txn_type = %row.transaction_type,
                    volume = row.volume,
                    price = row.price,
                    currency = %row.currency,
                    amendment = row.is_correction,
                    "published UnifiedInsiderTransaction (FI)"
                );
            }
        }
    }
}

#[cfg(test)]
mod fi_row_conversion_tests {
    use super::*;

    fn sample_row(is_correction: bool) -> FiRow {
        FiRow {
            ticker: "GOMX".to_string(),
            publication_date: parse_fi_datetime("2024-03-18 09:00:00").unwrap(),
            lei: "549300ABCDEF1234567".to_string(),
            reporting_person: "Jane Doe".to_string(),
            role: "VD".to_string(),
            is_close_associate: false,
            is_correction,
            correction_description: if is_correction {
                "Corrected volume".to_string()
            } else {
                String::new()
            },
            transaction_type: "Förvärv".to_string(),
            instrument_type: "Aktie".to_string(),
            isin: "SE0008348304".to_string(),
            transaction_date: parse_fi_datetime("2024-03-15 00:00:00").unwrap(),
            volume: 1500.0,
            price: 8.75,
            currency: "SEK".to_string(),
        }
    }

    #[test]
    fn converts_original_transaction_with_no_amendment_id() {
        let txn: UnifiedInsiderTransaction = sample_row(false).into();

        assert_eq!(txn.ticker, "GOMX");
        assert_eq!(txn.exchange_mic, "FNSE");
        assert_eq!(txn.source_registry, SourceRegistry::Fi as i32);
        assert_eq!(txn.person_name, "Jane Doe");
        assert_eq!(txn.person_role, "VD");
        assert_eq!(txn.transaction_date, "2024-03-15");
        assert_eq!(txn.published_date, "2024-03-18");
        assert_eq!(txn.transaction_type, UnifiedTransactionType::Buy as i32);
        assert_eq!(txn.volume, 1500.0);
        assert_eq!(txn.price_per_unit, 8.75);
        assert_eq!(txn.currency, "SEK");
        assert!(!txn.is_amendment);
        assert!(txn.amended_transaction_id.is_empty());

        match txn.source_detail {
            Some(unified_insider_transaction::SourceDetail::Fi(detail)) => {
                assert_eq!(detail.lei, "549300ABCDEF1234567");
                assert_eq!(detail.isin, "SE0008348304");
                assert_eq!(detail.instrument_type, "Aktie");
                assert!(!detail.is_close_associate);
            }
            other => panic!("expected FiDetail, got {other:?}"),
        }
    }

    #[test]
    fn correction_gets_a_non_empty_amendment_id_matching_the_original() {
        let original: UnifiedInsiderTransaction = sample_row(false).into();
        let correction: UnifiedInsiderTransaction = sample_row(true).into();

        assert!(correction.is_amendment);
        assert!(!correction.amended_transaction_id.is_empty());

        // The amendment id is derived from (source, ticker, person, transaction_date),
        // none of which differ between the original and its correction, so a
        // consumer keyed on this id will correctly overwrite the original.
        let expected_id = transaction_id(
            "FI",
            &original.ticker,
            &original.person_name,
            &original.transaction_date,
        );
        assert_eq!(correction.amended_transaction_id, expected_id);
    }

    #[test]
    fn sell_karaktar_maps_to_sell_transaction_type() {
        let mut row = sample_row(false);
        row.transaction_type = "Avyttring".to_string();
        let txn: UnifiedInsiderTransaction = row.into();
        assert_eq!(txn.transaction_type, UnifiedTransactionType::Sell as i32);
    }
}
