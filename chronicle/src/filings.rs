mod kafka;

use std::collections::HashMap;

use chrono::{Duration, NaiveDate, Utc};
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

/// Raw fields extracted from a single SEC Form 4 non-derivative transaction,
/// before conversion into the wire-level `UnifiedInsiderTransaction`. Keeping
/// parsing decoupled from the proto type means `parse_form4` doesn't need to
/// know about `SourceDetail`/`SecDetail` wrapping at all.
#[derive(Debug, Clone)]
struct SecFormFourTransaction {
    ticker: String,
    exchange_mic: String,
    person_name: String,
    person_role: String,
    transaction_date: String,
    published_date: String,
    transaction_type: i32,
    volume: f64,
    price_per_unit: f64,
    currency: String,
    issuer_cik: String,
    filer_cik: String,
    raw_transaction_code: String,
    filing_url: String,
}

impl From<SecFormFourTransaction> for UnifiedInsiderTransaction {
    fn from(r: SecFormFourTransaction) -> Self {
        UnifiedInsiderTransaction {
            ticker: r.ticker,
            exchange_mic: r.exchange_mic,
            source_registry: SourceRegistry::Sec as i32,
            person_name: r.person_name,
            person_role: r.person_role,
            transaction_date: r.transaction_date,
            published_date: r.published_date,
            transaction_type: r.transaction_type,
            volume: r.volume,
            price_per_unit: r.price_per_unit,
            currency: r.currency,
            is_amendment: false,
            amended_transaction_id: String::new(),
            source_detail: Some(unified_insider_transaction::SourceDetail::Sec(SecDetail {
                issuer_cik: r.issuer_cik,
                filer_cik: r.filer_cik,
                raw_transaction_code: r.raw_transaction_code,
                filing_url: r.filing_url,
            })),
        }
    }
}

/// Wire-format shape of an SEC Form 4 XML document (`<ownershipDocument>`),
/// covering only what this ingestion path needs. Mirrors the same "raw wire
/// struct, deserialized via serde" pattern already used on the Saxo side
/// (`alpha::saxo::stream::QuotePayload`, `alpha::saxo::uic::InstrumentSummary`)
/// instead of a hand-rolled XML event loop.
#[derive(Debug, serde::Deserialize)]
struct OwnershipDocument {
    #[serde(rename = "reportingOwner", default)]
    reporting_owner: Vec<ReportingOwner>,
    #[serde(rename = "nonDerivativeTable", default)]
    non_derivative_table: Option<NonDerivativeTable>,
}

#[derive(Debug, serde::Deserialize)]
struct ReportingOwner {
    #[serde(rename = "reportingOwnerId")]
    id: ReportingOwnerId,
    #[serde(rename = "reportingOwnerRelationship")]
    relationship: ReportingOwnerRelationship,
}

#[derive(Debug, serde::Deserialize)]
struct ReportingOwnerId {
    #[serde(rename = "rptOwnerCik")]
    cik: String,
    #[serde(rename = "rptOwnerName")]
    name: String,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ReportingOwnerRelationship {
    #[serde(rename = "isDirector", default)]
    is_director: String,
    #[serde(rename = "isOfficer", default)]
    is_officer: String,
    #[serde(rename = "isTenPercentOwner", default)]
    is_ten_percent_owner: String,
}

impl ReportingOwnerRelationship {
    fn role(&self) -> &'static str {
        if self.is_director.trim() == "1" {
            "director"
        } else if self.is_officer.trim() == "1" {
            "officer"
        } else if self.is_ten_percent_owner.trim() == "1" {
            "10-percent-owner"
        } else {
            "other"
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct NonDerivativeTable {
    #[serde(rename = "nonDerivativeTransaction", default)]
    transaction: Vec<NonDerivativeTransaction>,
}

#[derive(Debug, serde::Deserialize)]
struct NonDerivativeTransaction {
    #[serde(rename = "transactionDate")]
    transaction_date: ValueField,
    #[serde(rename = "transactionCoding")]
    transaction_coding: TransactionCoding,
    #[serde(rename = "transactionAmounts")]
    transaction_amounts: TransactionAmounts,
}

#[derive(Debug, serde::Deserialize)]
struct TransactionCoding {
    #[serde(rename = "transactionCode")]
    transaction_code: String,
}

#[derive(Debug, serde::Deserialize)]
struct TransactionAmounts {
    #[serde(rename = "transactionShares")]
    transaction_shares: ValueField,
    #[serde(rename = "transactionPricePerShare", default)]
    transaction_price_per_share: Option<ValueField>,
}

/// SEC's Form 4 schema wraps most atomic values in a `<value>` child element
/// (sibling to an optional `<footnoteId>`), rather than as the parent
/// element's own text content.
#[derive(Debug, serde::Deserialize)]
struct ValueField {
    value: String,
}

/// Parse a Form 4 XML document and return raw `SecFormFourTransaction` records
/// for P (purchase) and S (sale) transactions.
fn parse_form4(
    xml: &str,
    ticker: &str,
    issuer_cik: &str,
    filing_date: NaiveDate,
) -> Vec<SecFormFourTransaction> {
    let doc: OwnershipDocument = match quick_xml::de::from_str(xml) {
        Ok(d) => d,
        Err(e) => {
            warn!(ticker = %ticker, "Form 4 XML parse error: {e}");
            return Vec::new();
        }
    };

    // Real Form 4 filings almost always list exactly one reporting owner;
    // joint filings can list more, but (matching this path's existing scope)
    // only the first is tracked as "the" filer for every transaction below.
    let Some(owner) = doc.reporting_owner.first() else {
        warn!(ticker = %ticker, "Form 4 filing has no reportingOwner, skipping");
        return Vec::new();
    };

    let filer_name = owner.id.name.trim().to_string();
    let filer_cik = owner.id.cik.trim().to_string();
    let filer_role = owner.relationship.role().to_string();
    let filing_date_str = filing_date.format("%Y-%m-%d").to_string();

    doc.non_derivative_table
        .map(|t| t.transaction)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|txn| {
            let code = txn.transaction_coding.transaction_code.trim().to_string();
            if code != "P" && code != "S" {
                return None;
            }

            let transaction_date = txn.transaction_date.value.trim().to_string();
            let shares = txn
                .transaction_amounts
                .transaction_shares
                .value
                .trim()
                .parse::<f64>()
                .unwrap_or(0.0);
            let price = txn
                .transaction_amounts
                .transaction_price_per_share
                .as_ref()
                .and_then(|v| v.value.trim().parse::<f64>().ok())
                .unwrap_or(0.0);

            let unified_type = match code.as_str() {
                "P" => model::generated::UnifiedTransactionType::Buy as i32,
                "S" => model::generated::UnifiedTransactionType::Sell as i32,
                _ => model::generated::UnifiedTransactionType::Other as i32,
            };

            // Computed for future use in `amended_transaction_id` once SEC
            // amendment (Form 4/A) handling is added.
            let _txn_id = transaction_id("SEC", ticker, &filer_name, &transaction_date);

            let filing_url = form4_xml_url(
                issuer_cik.trim_start_matches('0'),
                // accession not available here; leave empty — caller sets it
                "",
            );

            Some(SecFormFourTransaction {
                ticker: ticker.to_string(),
                exchange_mic: "XNAS".to_string(), // SEC tickers default to XNAS; signal updates per registry
                person_name: filer_name.clone(),
                person_role: filer_role.clone(),
                transaction_date,
                published_date: filing_date_str.clone(),
                transaction_type: unified_type,
                volume: shares,
                price_per_unit: price,
                currency: "USD".to_string(),
                issuer_cik: issuer_cik.to_string(),
                filer_cik: filer_cik.clone(),
                raw_transaction_code: code,
                filing_url,
            })
        })
        .collect()
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

            for raw_filing in filings {
                let filing = model::generated::UnifiedInsiderTransaction::from(raw_filing);
                if let Err(e) = producer
                    .publish_unified_insider_transaction(&args.kafka_topic, &filing)
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

#[cfg(test)]
mod parse_form4_tests {
    use super::*;

    /// A trimmed-but-schema-accurate SEC Form 4 XML body: one reporting owner
    /// (an officer), one qualifying open-market purchase ("P"), and one
    /// non-qualifying transaction code ("A", an award/grant) that must be
    /// filtered out.
    const SAMPLE_FORM4_XML: &str = r#"
<ownershipDocument>
    <reportingOwner>
        <reportingOwnerId>
            <rptOwnerCik>0001234567</rptOwnerCik>
            <rptOwnerName>Jane Doe</rptOwnerName>
        </reportingOwnerId>
        <reportingOwnerRelationship>
            <isDirector>0</isDirector>
            <isOfficer>1</isOfficer>
            <isTenPercentOwner>0</isTenPercentOwner>
            <officerTitle>Chief Executive Officer</officerTitle>
        </reportingOwnerRelationship>
    </reportingOwner>
    <nonDerivativeTable>
        <nonDerivativeTransaction>
            <securityTitle>
                <value>Common Stock</value>
            </securityTitle>
            <transactionDate>
                <value>2024-03-15</value>
            </transactionDate>
            <transactionCoding>
                <transactionFormType>4</transactionFormType>
                <transactionCode>P</transactionCode>
            </transactionCoding>
            <transactionAmounts>
                <transactionShares>
                    <value>1000</value>
                </transactionShares>
                <transactionPricePerShare>
                    <value>12.5</value>
                </transactionPricePerShare>
            </transactionAmounts>
        </nonDerivativeTransaction>
        <nonDerivativeTransaction>
            <securityTitle>
                <value>Common Stock</value>
            </securityTitle>
            <transactionDate>
                <value>2024-03-16</value>
            </transactionDate>
            <transactionCoding>
                <transactionFormType>4</transactionFormType>
                <transactionCode>A</transactionCode>
            </transactionCoding>
            <transactionAmounts>
                <transactionShares>
                    <value>500</value>
                </transactionShares>
                <transactionPricePerShare>
                    <value>0</value>
                </transactionPricePerShare>
            </transactionAmounts>
        </nonDerivativeTransaction>
    </nonDerivativeTable>
</ownershipDocument>
"#;

    #[test]
    fn parses_qualifying_purchase_and_filters_non_qualifying_code() {
        let filing_date = NaiveDate::from_ymd_opt(2024, 3, 18).unwrap();
        let results = parse_form4(SAMPLE_FORM4_XML, "ACME", "0000012345", filing_date);

        // The "A" (award/grant) transaction must be filtered out — only P/S qualify.
        assert_eq!(results.len(), 1);
        let txn = &results[0];

        assert_eq!(txn.ticker, "ACME");
        assert_eq!(txn.exchange_mic, "XNAS");
        assert_eq!(txn.person_name, "Jane Doe");
        assert_eq!(txn.person_role, "officer");
        assert_eq!(txn.transaction_date, "2024-03-15");
        assert_eq!(txn.published_date, "2024-03-18");
        assert_eq!(
            txn.transaction_type,
            model::generated::UnifiedTransactionType::Buy as i32
        );
        assert_eq!(txn.volume, 1000.0);
        assert_eq!(txn.price_per_unit, 12.5);
        assert_eq!(txn.currency, "USD");
        assert_eq!(txn.issuer_cik, "0000012345");
        assert_eq!(txn.filer_cik, "0001234567");
        assert_eq!(txn.raw_transaction_code, "P");
    }

    #[test]
    fn missing_reporting_owner_yields_no_transactions() {
        let xml = r#"<ownershipDocument><nonDerivativeTable/></ownershipDocument>"#;
        let filing_date = NaiveDate::from_ymd_opt(2024, 3, 18).unwrap();
        let results = parse_form4(xml, "ACME", "0000012345", filing_date);
        assert!(results.is_empty());
    }

    #[test]
    fn missing_price_per_share_defaults_to_zero_rather_than_erroring() {
        let xml = r#"
<ownershipDocument>
    <reportingOwner>
        <reportingOwnerId>
            <rptOwnerCik>0001234567</rptOwnerCik>
            <rptOwnerName>Jane Doe</rptOwnerName>
        </reportingOwnerId>
        <reportingOwnerRelationship>
            <isDirector>1</isDirector>
            <isOfficer>0</isOfficer>
            <isTenPercentOwner>0</isTenPercentOwner>
        </reportingOwnerRelationship>
    </reportingOwner>
    <nonDerivativeTable>
        <nonDerivativeTransaction>
            <transactionDate><value>2024-03-15</value></transactionDate>
            <transactionCoding>
                <transactionCode>S</transactionCode>
            </transactionCoding>
            <transactionAmounts>
                <transactionShares><value>200</value></transactionShares>
            </transactionAmounts>
        </nonDerivativeTransaction>
    </nonDerivativeTable>
</ownershipDocument>
"#;
        let filing_date = NaiveDate::from_ymd_opt(2024, 3, 18).unwrap();
        let results = parse_form4(xml, "ACME", "0000012345", filing_date);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].price_per_unit, 0.0);
        assert_eq!(results[0].person_role, "director");
        assert_eq!(
            results[0].transaction_type,
            model::generated::UnifiedTransactionType::Sell as i32
        );
    }
}
