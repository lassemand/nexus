/// Fetch PDMR (insider) transactions from Finansinspektionen (FI) and
/// publish them as `InsiderFiling` protobuf messages to Kafka.
///
/// Only tickers registered with `exchange_mic = "FNSE"` (Nasdaq First North
/// Growth Market Stockholm) are processed — US-listed tickers are handled
/// by the `filings` binary (SEC Form 4).
///
/// # Data source
///
/// FI's MAR Article 19 PDMR register — public, no authentication required.
/// See `docs/adr/0002-fi-pdmr-register-access.md` for protocol details.
///
/// Endpoint:
/// `GET https://marknadssok.fi.se/Publiceringsklient/sv-SE/Search/Search
///      ?SearchFunctionType=Insyn&Utgivare={issuer}&Transaktionsdatum.From={from}
///      &Transaktionsdatum.To={to}&button=export`
///
/// Response: UTF-16 LE, semicolon-delimited CSV.
mod kafka;

use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use clap::Parser;
use kafka::{load_ticker_registrations, ChronicleProducer};
use model::{asset::mic, generated::InsiderFiling};
use tracing::{error, info, warn};

const FI_BASE: &str = "https://marknadssok.fi.se/Publiceringsklient/sv-SE/Search/Search";
const FI_AUTOCOMPLETE: &str =
    "https://marknadssok.fi.se/Publiceringsklient/sv-SE/AutoComplete/H\u{00e4}mtaAutoCompleteListaFull";

#[derive(Parser)]
#[command(about = "Fetch FI PDMR insider transactions and publish as InsiderFiling to Kafka")]
struct Args {
    #[arg(long, env = "KAFKA_BROKERS")]
    kafka_brokers: String,

    #[arg(long, env = "KAFKA_TOPIC", default_value = "insider.filings")]
    kafka_topic: String,

    #[arg(long, env = "LOOKBACK_DAYS", default_value = "90")]
    lookback_days: i64,

    #[arg(long, env = "KAFKA_TICKERS_TOPIC", default_value = "company.tickers")]
    tickers_topic: String,
}

/// Resolve a ticker symbol to the exact issuer name used by FI via autocomplete.
/// Returns the first match, or `None` if no match is found.
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

/// A single PDMR transaction row parsed from FI's CSV.
struct PdmrRow {
    publication_date: NaiveDateTime,
    lei: String,
    reporting_person: String,
    role: String,
    /// Whether this transaction was filed by a close associate of the PDMR
    /// rather than the PDMR themselves. Logged for observability.
    #[allow(dead_code)]
    is_close_associate: bool,
    /// Whether this row is a correction of a previously published filing.
    is_correction: bool,
    transaction_type: String,
    /// Instrument type (e.g. "Aktie", "Warrant"). Logged for observability.
    #[allow(dead_code)]
    instrument_type: String,
    /// ISIN of the instrument. Logged for observability.
    #[allow(dead_code)]
    isin: String,
    transaction_date: NaiveDateTime,
    volume: f64,
    price: f64,
    currency: String,
}

/// Map FI transaction type (`Karaktär`) to InsiderFiling transaction code.
/// "P" = purchase/acquisition, "S" = sale/disposal, "O" = other.
fn map_transaction_code(karaktar: &str) -> &'static str {
    match karaktar {
        k if k.contains("Förvärv") => "P",
        k if k.contains("Avyttring") => "S",
        _ => "O",
    }
}

/// Map FI role (`Befattning`) to a normalised role string.
fn map_role(befattning: &str) -> String {
    let lower = befattning.to_lowercase();
    if lower.contains("vd") || lower.contains("verkställande direktör") || lower.contains("ceo") {
        "CEO".to_string()
    } else if lower.contains("styrelseordf") {
        "Chairman".to_string()
    } else if lower.contains("styrelse") || lower.contains("ledningsorgan") {
        "Board member".to_string()
    } else if lower.contains("ledande") {
        "Senior executive".to_string()
    } else {
        "Other".to_string()
    }
}

/// Parse a Swedish-locale decimal string (comma as decimal separator) into f64.
fn parse_swedish_f64(s: &str) -> f64 {
    s.trim().replace(',', ".").parse().unwrap_or(0.0)
}

/// Parse a FI datetime string `YYYY-MM-DD HH:MM:SS` into a NaiveDateTime.
fn parse_fi_datetime(s: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S").ok()
}

/// Fetch and parse PDMR transactions for a given FI issuer name.
async fn fetch_pdmr(
    client: &reqwest::Client,
    issuer: &str,
    from: NaiveDate,
    to: NaiveDate,
) -> anyhow::Result<Vec<PdmrRow>> {
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

    // Response is UTF-16 LE, semicolon-delimited CSV.
    let text = String::from_utf16_lossy(
        &bytes
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect::<Vec<_>>(),
    );

    let mut rows = Vec::new();
    let mut lines = text.lines();

    // Skip header row.
    let Some(header) = lines.next() else {
        return Ok(rows);
    };

    // Build column index map from the semicolon-split header.
    let cols: Vec<&str> = header.split(';').collect();
    let idx = |name: &str| cols.iter().position(|&c| c.trim() == name);

    let i_pub = idx("Publiceringsdatum").unwrap_or(0);
    let i_lei = idx("LEI-kod").unwrap_or(2);
    let i_rep = idx("Anmälningsskyldig").unwrap_or(3);
    let i_role = idx("Befattning").unwrap_or(5);
    let i_near = idx("Närstående").unwrap_or(6);
    let i_corr = idx("Korrigering").unwrap_or(7);
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

        // Skip cancelled/superseded rows.
        if get(i_status) != "Aktuell" {
            continue;
        }

        let Some(pub_dt) = parse_fi_datetime(get(i_pub)) else {
            continue;
        };
        let Some(txn_dt) = parse_fi_datetime(get(i_date)) else {
            continue;
        };

        rows.push(PdmrRow {
            publication_date: pub_dt,
            lei: get(i_lei).to_string(),
            reporting_person: get(i_rep).to_string(),
            role: get(i_role).to_string(),
            is_close_associate: !get(i_near).is_empty(),
            is_correction: !get(i_corr).is_empty(),
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

#[tokio::main]
async fn main() {
    dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/.env")).ok();
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    // Load only FNSE-listed tickers — US tickers are handled by the filings binary.
    let registrations = load_ticker_registrations(&args.kafka_brokers, &args.tickers_topic);
    let fnse_tickers: Vec<String> = registrations
        .into_values()
        .filter(|r| r.exchange_mic == mic::FNSE || r.exchange_mic.is_empty())
        .map(|r| r.ticker.to_uppercase())
        .collect();

    if fnse_tickers.is_empty() {
        warn!(
            "no FNSE tickers registered — register tickers via: \
             nexus register <TICKER> --exchange-mic FNSE"
        );
        return;
    }

    info!(count = fnse_tickers.len(), "loaded FNSE tickers from kafka");

    let client = reqwest::Client::builder()
        .user_agent("nexus lasse.alm@gsfleet.io")
        .build()
        .expect("failed to build HTTP client");

    let producer = ChronicleProducer::new(&args.kafka_brokers).expect("failed to connect to Kafka");

    let to = Utc::now().date_naive();
    // FI MAR data starts 2016-07-03; never query before that date.
    let mar_start = NaiveDate::from_ymd_opt(2016, 7, 3).unwrap();
    let from = (to - Duration::days(args.lookback_days)).max(mar_start);

    for (i, ticker) in fnse_tickers.iter().enumerate() {
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

        info!(ticker, issuer, "fetching PDMR transactions from FI");

        let rows = match fetch_pdmr(&client, &issuer, from, to).await {
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
            // Skip non-share instruments if desired (uncomment to restrict to Aktie only).
            // if row.instrument_type != "Aktie" { continue; }

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

            let filing = InsiderFiling {
                ticker: ticker.clone(),
                issuer_cik: row.lei.clone(), // FI uses LEI; stored in issuer_cik field
                filer_name: row.reporting_person.clone(),
                filer_role: map_role(&row.role),
                transaction_date_unix_secs: txn_unix,
                filing_date_unix_secs: pub_unix,
                shares: row.volume,
                price_per_share: row.price,
                transaction_code: map_transaction_code(&row.transaction_type).to_string(),
                filer_cik: String::new(), // FI has no CIK equivalent
            };

            if let Err(e) = producer.publish(&args.kafka_topic, ticker, &filing).await {
                error!(ticker, error = %e, "failed to publish InsiderFiling");
            } else {
                info!(
                    ticker,
                    filer = %row.reporting_person,
                    txn_type = %row.transaction_type,
                    shares = row.volume,
                    price = row.price,
                    currency = %row.currency,
                    correction = row.is_correction,
                    "published PDMR filing"
                );
            }
        }
    }
}
