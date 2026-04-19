use chrono::{Datelike, NaiveDate, TimeZone, Utc};

/// Parses an EDGAR date string ("YYYY-MM-DD") into a UTC unix timestamp (seconds).
pub fn filed_at_to_unix(date_str: &str) -> Option<i64> {
    let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d").ok()?;
    Some(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?).timestamp())
}

/// Derives fiscal quarter (1–4) from a report period date string ("YYYY-MM-DD").
pub fn fiscal_quarter(date_str: &str) -> Option<u32> {
    let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d").ok()?;
    Some(((date.month() - 1) / 3) + 1)
}

/// Derives fiscal year from a report period date string ("YYYY-MM-DD").
pub fn fiscal_year(date_str: &str) -> Option<u32> {
    let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d").ok()?;
    Some(date.year() as u32)
}

/// Constructs the EDGAR filing index URL from CIK and accession number.
pub fn filing_url(cik: u64, accession_number: &str) -> String {
    let no_dashes = accession_number.replace('-', "");
    format!(
        "https://www.sec.gov/Archives/edgar/data/{cik}/{no_dashes}/{accession_number}-index.htm"
    )
}
