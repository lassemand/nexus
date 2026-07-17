mod kafka;

use std::collections::HashMap;

use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use clap::Parser;
use kafka::{load_tickers, ChronicleProducer};
use model::{
    generated::{
        unified_insider_transaction, SecDetail, SourceRegistry, UnifiedInsiderTransaction,
    },
    insider::transaction_id,
};
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(
    about = "Fetch SEC Form 4 open-market purchases and publish as UnifiedInsiderTransaction to Kafka"
)]
struct Args {
    #[arg(long, env = "KAFKA_BROKERS")]
    kafka_brokers: String,

    #[arg(long, env = "KAFKA_TOPIC", default_value = "insider.transactions")]
    kafka_topic: String,

    #[arg(long, env = "LOOKBACK_DAYS", default_value = "90")]
    lookback_days: i64,

    #[arg(long, env = "KAFKA_TICKERS_TOPIC", default_value = "company.tickers")]
    tickers_topic: String,
}

/// Build the ticker → zero-padded 10-digit CIK map from EDGAR.
async fn fetch_cik_map(client: &reqwest::Client) -> anyhow::Result<HashMap<String, String>> {
    #[derive(serde::Deserialize)]
    struct Entry {
        cik_str: u64,
        ticker: String,
    }

    let raw: HashMap<String, Entry> = client
        .get("https://www.sec.gov/files/company_tickers.json")
        .send()
        .await?
        .json()
        .await?;

    Ok(raw
        .into_values()
        .map(|e| (e.ticker.to_uppercase(), format!("{:010}", e.cik_str)))
        .collect())
}

/// Fetch the EDGAR submissions JSON for a CIK and return recent Form 4 accession numbers.
async fn form4_accessions(
    client: &reqwest::Client,
    cik: &str,
    cutoff: NaiveDate,
) -> anyhow::Result<Vec<(String, NaiveDate)>> {
    #[derive(serde::Deserialize)]
    struct Recent {
        form: Vec<String>,
        #[serde(rename = "filingDate")]
        filing_date: Vec<String>,
        #[serde(rename = "accessionNumber")]
        accession_number: Vec<String>,
    }
    #[derive(serde::Deserialize)]
    struct Filings {
        recent: Recent,
    }
    #[derive(serde::Deserialize)]
    struct Submissions {
        filings: Filings,
    }

    let url = format!("https://data.sec.gov/submissions/CIK{cik}.json");
    let subs: Submissions = client.get(&url).send().await?.json().await?;

    let recent = subs.filings.recent;
    let mut result = Vec::new();
    for i in 0..recent.form.len() {
        if recent.form[i] != "4" {
            continue;
        }
        let date = match NaiveDate::parse_from_str(&recent.filing_date[i], "%Y-%m-%d") {
            Ok(d) => d,
            Err(_) => continue,
        };
        if date < cutoff {
            continue;
        }
        result.push((recent.accession_number[i].clone(), date));
    }
    Ok(result)
}

/// Extract the XML block from an EDGAR .txt filing.
///
/// EDGAR wraps the actual form XML inside `<XML>...</XML>` tags, preceded by a
/// non-standard SEC header that is not valid XML. This strips everything outside
/// the first `<XML>...</XML>` block so the parser only sees well-formed content.
fn extract_xml_block(raw: &str) -> &str {
    let start = raw.find("<XML>").map(|i| i + 5).unwrap_or(0);
    let end = raw.find("</XML>").unwrap_or(raw.len());
    raw[start..end].trim()
}

/// Parse a Form 4 XML document and return `UnifiedInsiderTransaction` records
/// for P (purchase) and S (sale) transactions.
fn parse_form4(
    xml: &str,
    ticker: &str,
    issuer_cik: &str,
    filing_date: NaiveDate,
) -> Vec<UnifiedInsiderTransaction> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let _filing_date_unix =
        NaiveDateTime::new(filing_date, NaiveTime::from_hms_opt(0, 0, 0).unwrap())
            .and_utc()
            .timestamp();

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    // Extract owner info (single per filing)
    let mut filer_name = String::new();
    let mut filer_cik = String::new();
    let mut filer_role = String::new();

    // Per-transaction state
    let mut in_non_deriv_txn = false;
    let mut txn_code = String::new();
    let mut txn_date_str = String::new();
    let mut txn_shares_str = String::new();
    let mut txn_price_str = String::new();

    // Flags set when we enter specific parent elements, cleared on exit.
    // Used to identify which <value> child we're reading without heuristics.
    let mut in_txn_date = false;
    let mut in_txn_shares = false;
    let mut in_txn_price = false;
    let mut current_tag = String::new();
    let mut in_relationship = false;
    let mut is_director = false;
    let mut is_officer = false;
    let mut is_ten_pct = false;

    let mut results = Vec::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Err(e) => {
                warn!("XML parse error: {e}");
                break;
            }
            Ok(Event::Eof) => break,

            Ok(Event::Start(e)) => {
                let name = std::str::from_utf8(e.name().as_ref())
                    .unwrap_or("")
                    .to_string();
                current_tag = name.clone();
                match name.as_str() {
                    "nonDerivativeTransaction" => {
                        in_non_deriv_txn = true;
                        txn_code.clear();
                        txn_date_str.clear();
                        txn_shares_str.clear();
                        txn_price_str.clear();
                    }
                    "transactionDate" => in_txn_date = true,
                    "transactionShares" => in_txn_shares = true,
                    "transactionPricePerShare" => in_txn_price = true,
                    "reportingOwnerRelationship" => {
                        in_relationship = true;
                        is_director = false;
                        is_officer = false;
                        is_ten_pct = false;
                    }
                    _ => {}
                }
            }

            Ok(Event::End(e)) => {
                let name_bytes = e.name();
                let name = std::str::from_utf8(name_bytes.as_ref()).unwrap_or("");
                match name {
                    "nonDerivativeTransaction" => {
                        let code = txn_code.trim().to_string();
                        if in_non_deriv_txn && (code == "P" || code == "S") {
                            let txn_date =
                                NaiveDate::parse_from_str(txn_date_str.trim(), "%Y-%m-%d").ok();
                            let _txn_date_unix = txn_date
                                .map(|d| {
                                    NaiveDateTime::new(d, NaiveTime::from_hms_opt(0, 0, 0).unwrap())
                                        .and_utc()
                                        .timestamp()
                                })
                                .unwrap_or(0);
                            let shares = txn_shares_str.trim().parse::<f64>().unwrap_or(0.0);
                            let price = txn_price_str.trim().parse::<f64>().unwrap_or(0.0);
                            let txn_date_str = txn_date_str.trim().to_string();
                            let filing_date_str = filing_date.format("%Y-%m-%d").to_string();

                            let unified_type = match code.as_str() {
                                "P" => model::generated::UnifiedTransactionType::Buy as i32,
                                "S" => model::generated::UnifiedTransactionType::Sell as i32,
                                _ => model::generated::UnifiedTransactionType::Other as i32,
                            };

                            let txn_id = transaction_id("SEC", ticker, &filer_name, &txn_date_str);

                            let filing_url = form4_xml_url(
                                issuer_cik.trim_start_matches('0'),
                                // accession not available here; leave empty — caller sets it
                                "",
                            );

                            results.push(UnifiedInsiderTransaction {
                                ticker: ticker.to_string(),
                                exchange_mic: "XNAS".to_string(), // SEC tickers default to XNAS; signal updates per registry
                                source_registry: SourceRegistry::Sec as i32,
                                person_name: filer_name.clone(),
                                person_role: filer_role.clone(),
                                transaction_date: txn_date_str,
                                published_date: filing_date_str,
                                transaction_type: unified_type,
                                volume: shares,
                                price_per_unit: price,
                                currency: "USD".to_string(),
                                is_amendment: false,
                                amended_transaction_id: String::new(),
                                source_detail: Some(
                                    unified_insider_transaction::SourceDetail::Sec(SecDetail {
                                        issuer_cik: issuer_cik.to_string(),
                                        filer_cik: filer_cik.clone(),
                                        raw_transaction_code: code,
                                        filing_url,
                                    }),
                                ),
                            });
                            let _ = txn_id; // computed for future use in amended_transaction_id
                        }
                        in_non_deriv_txn = false;
                    }
                    "transactionDate" => in_txn_date = false,
                    "transactionShares" => in_txn_shares = false,
                    "transactionPricePerShare" => in_txn_price = false,
                    "reportingOwnerRelationship" => {
                        filer_role = if is_director {
                            "director"
                        } else if is_officer {
                            "officer"
                        } else if is_ten_pct {
                            "10-percent-owner"
                        } else {
                            "other"
                        }
                        .to_string();
                        in_relationship = false;
                    }
                    _ => {}
                }
                current_tag.clear();
            }

            Ok(Event::Text(e)) => {
                let text = e.unescape().unwrap_or_default().to_string();
                if in_non_deriv_txn {
                    match current_tag.as_str() {
                        "transactionCode" => txn_code = text.clone(),
                        "value" if in_txn_date => txn_date_str = text.clone(),
                        "value" if in_txn_shares => txn_shares_str = text.clone(),
                        "value" if in_txn_price => txn_price_str = text.clone(),
                        _ => {}
                    }
                }
                // These tags appear outside transactions too
                match current_tag.as_str() {
                    "rptOwnerName" if filer_name.is_empty() => filer_name = text.clone(),
                    "rptOwnerCik" if filer_cik.is_empty() => filer_cik = text.clone(),
                    "isDirector" if in_relationship => is_director = text.trim() == "1",
                    "isOfficer" if in_relationship => is_officer = text.trim() == "1",
                    "isTenPercentOwner" if in_relationship => is_ten_pct = text.trim() == "1",
                    _ => {}
                }
            }
            _ => {}
        }
        buf.clear();
    }
    results
}

/// EDGAR XML URL from accession number (e.g. "0001234567-24-000001").
fn form4_xml_url(cik: &str, accession: &str) -> String {
    let acc_nodash = accession.replace('-', "");
    // The primary document is typically named like "wf-form4_*.xml"; fall back to
    // listing-based resolution. For simplicity we use the accession index page
    // naming convention: <accession-no-dashes>/<cik>-<date>-form4.xml — but EDGAR
    // doesn't guarantee this. The reliable path is the filing index.
    // We use the known pattern: /Archives/edgar/data/<cik>/<acc_nodash>/<acc>.xml
    format!(
        "https://www.sec.gov/Archives/edgar/data/{}/{}/{}.txt",
        cik.trim_start_matches('0'),
        acc_nodash,
        accession
    )
}

#[tokio::main]
async fn main() {
    dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/.env")).ok();
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    let tickers = load_tickers(&args.kafka_brokers, &args.tickers_topic);

    if tickers.is_empty() {
        warn!("no tickers registered — publish a TickerRegistration to the company.tickers topic first");
        return;
    }

    info!(count = tickers.len(), "loaded tickers from kafka");

    let client = reqwest::Client::builder()
        .user_agent("nexus lasse.alm@gsfleet.io")
        .build()
        .expect("failed to build HTTP client");

    info!("fetching ticker→CIK map from EDGAR");
    let cik_map = match fetch_cik_map(&client).await {
        Ok(m) => m,
        Err(e) => {
            error!("failed to fetch CIK map: {e}");
            return;
        }
    };

    let producer = ChronicleProducer::new(&args.kafka_brokers).expect("failed to connect to Kafka");

    let cutoff = Utc::now().date_naive() - Duration::days(args.lookback_days);

    for (i, ticker) in tickers.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        let cik = match cik_map.get(ticker.as_str()) {
            Some(c) => c.clone(),
            None => {
                warn!(ticker = %ticker, "CIK not found, skipping");
                continue;
            }
        };

        let accessions = match form4_accessions(&client, &cik, cutoff).await {
            Ok(a) => a,
            Err(e) => {
                error!(ticker = %ticker, error = %e, "failed to fetch Form 4 accessions");
                continue;
            }
        };

        info!(ticker = %ticker, count = accessions.len(), "Form 4 filings in window");

        for (accession, filing_date) in &accessions {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;

            let url = form4_xml_url(&cik, accession);
            let xml = match client.get(&url).send().await {
                Ok(r) => match r.text().await {
                    Ok(t) => t,
                    Err(e) => {
                        error!(ticker = %ticker, accession = %accession, error = %e, "failed to read Form 4 body");
                        continue;
                    }
                },
                Err(e) => {
                    error!(ticker = %ticker, accession = %accession, error = %e, "failed to fetch Form 4 XML");
                    continue;
                }
            };

            // EDGAR .txt filings wrap the actual XML in <XML>...</XML> tags,
            // preceded by a non-XML SEC header that confuses strict XML parsers.
            // Extract only the XML portion before parsing.
            let xml_content = extract_xml_block(&xml);
            let filings = parse_form4(xml_content, ticker, &cik, *filing_date);

            for filing in &filings {
                if let Err(e) = producer
                    .publish_unified_insider_transaction(&args.kafka_topic, filing)
                    .await
                {
                    error!(ticker = %ticker, error = %e, "failed to publish UnifiedInsiderTransaction");
                } else {
                    info!(
                        ticker = %ticker,
                        filer = %filing.person_name,
                        volume = filing.volume,
                        price = filing.price_per_unit,
                        "published UnifiedInsiderTransaction (SEC)"
                    );
                }
            }
        }
    }
}
