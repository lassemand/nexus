use reqwest::Client;
use serde::Deserialize;
use thiserror::Error;

const EDGAR_BASE: &str = "https://data.sec.gov";
const USER_AGENT: &str = "nexus-chronicle/0.1 contact@example.com";

#[derive(Debug, Error)]
pub enum EdgarError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("company CIK not found for ticker {0}")]
    CikNotFound(String),
}

/// A single SEC filing entry.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Filing {
    pub accession_number: String,
    pub form: String,
    pub filed_at: String,
    pub report_date: String,
}

#[derive(Deserialize)]
struct TickerEntry {
    cik_str: u64,
}

#[derive(Deserialize)]
struct Submissions {
    filings: FilingsWrapper,
}

#[derive(Deserialize)]
struct FilingsWrapper {
    recent: RecentFilings,
}

#[derive(Deserialize)]
struct RecentFilings {
    #[serde(rename = "accessionNumber")]
    accession_number: Vec<String>,
    form: Vec<String>,
    #[serde(rename = "filingDate")]
    filing_date: Vec<String>,
    #[serde(rename = "reportDate")]
    report_date: Vec<String>,
}

pub struct EdgarClient {
    http: Client,
}

impl EdgarClient {
    pub fn new() -> Result<Self, EdgarError> {
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .build()?;
        Ok(Self { http })
    }

    /// Resolves a ticker symbol to its SEC CIK number.
    pub async fn cik_for_ticker(&self, ticker: &str) -> Result<u64, EdgarError> {
        let url = format!("{EDGAR_BASE}/submissions/CIK{:010}.json", 0);
        // Use the company tickers JSON published by SEC.
        let tickers: serde_json::Value = self
            .http
            .get("https://www.sec.gov/files/company_tickers.json")
            .send()
            .await?
            .json()
            .await?;

        let upper = ticker.to_uppercase();
        for entry in tickers.as_object().into_iter().flatten() {
            let t: TickerEntry = serde_json::from_value(entry.1.clone())
                .unwrap_or(TickerEntry { cik_str: 0 });
            if entry.1["ticker"].as_str().unwrap_or("").to_uppercase() == upper {
                return Ok(t.cik_str);
            }
        }
        let _ = url;
        Err(EdgarError::CikNotFound(ticker.to_string()))
    }

    /// Returns all 8-K filings for a company identified by CIK.
    pub async fn earnings_filings(&self, cik: u64) -> Result<Vec<Filing>, EdgarError> {
        let url = format!("{EDGAR_BASE}/submissions/CIK{cik:010}.json");
        let subs: Submissions = self.http.get(&url).send().await?.json().await?;

        let recent = subs.filings.recent;
        let filings = recent
            .form
            .iter()
            .enumerate()
            .filter(|(_, form)| *form == "8-K")
            .map(|(i, form)| Filing {
                accession_number: recent.accession_number[i].clone(),
                form: form.clone(),
                filed_at: recent.filing_date[i].clone(),
                report_date: recent.report_date[i].clone(),
            })
            .collect();

        Ok(filings)
    }
}

impl Default for EdgarClient {
    fn default() -> Self {
        Self::new().expect("failed to build EdgarClient")
    }
}
