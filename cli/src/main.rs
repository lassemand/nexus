use anyhow::{Context, Result};
use chrono::{Datelike, NaiveDate, Weekday};
use clap::{Parser, Subcommand};
use futures::future::join_all;
use model::generated::TickerRegistration;
use prost::Message;
use rdkafka::{
    producer::{FutureProducer, FutureRecord},
    ClientConfig,
};
use serde::Deserialize;
use std::time::Duration;

/// Nexus CLI — interact with the nexus insider-signal pipeline.
#[derive(Parser)]
#[command(
    name = "nexus",
    about = "Nexus CLI — interact with the nexus insider-signal pipeline"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Register one or more tickers for signal tracking.
    ///
    /// Validates each ticker against Yahoo Finance before publishing a
    /// TickerRegistration protobuf message to the configured Kafka topic.
    /// All tickers are attempted even if earlier ones fail; the process
    /// exits with code 1 if any validation or delivery fails.
    Register {
        /// Ticker symbols to register (e.g. AAPL MSFT GOOG).
        #[arg(required = true)]
        tickers: Vec<String>,

        /// ISO 10383 Market Identifier Code for the exchange these tickers
        /// trade on. Defaults to XNAS (Nasdaq US). Use FNSE for Nasdaq First
        /// North Growth Market Stockholm, XNYS for NYSE, etc.
        #[arg(long, env = "EXCHANGE_MIC", default_value = "XNAS")]
        exchange_mic: String,

        /// Kafka broker addresses.
        #[arg(long, env = "KAFKA_BROKERS", default_value = "localhost:9092")]
        brokers: String,

        /// Kafka topic for ticker registrations.
        #[arg(long, env = "KAFKA_TICKERS_TOPIC", default_value = "company.tickers")]
        topic: String,
    },

    /// Fetch and store trading holiday data for an exchange.
    ///
    /// Fetches public holiday data from the Nager.Date API
    /// (https://date.nager.at) and maps it to exchange-specific closure
    /// rules, then upserts the results into the trading_holidays table.
    ///
    /// Example: nexus calendar sync --exchange FNSE --year 2028
    #[command(subcommand_required = true)]
    Calendar {
        #[command(subcommand)]
        action: CalendarAction,
    },

    /// Print shell completion scripts.
    ///
    /// Install with:
    ///   source <(nexus completions zsh)
    /// or copy to ~/.zsh/completions/_nexus and restart your shell.
    Completions {
        #[command(subcommand)]
        shell: CompletionsShell,
    },
}

#[derive(Subcommand)]
enum CalendarAction {
    /// Fetch trading holidays for a year from Nager.Date and upsert into the DB.
    Sync {
        /// ISO 10383 MIC of the exchange to sync. Currently only FNSE
        /// (Nasdaq First North Growth Market Stockholm) is supported.
        #[arg(long, default_value = "FNSE")]
        exchange: String,

        /// Calendar year to fetch (e.g. 2028).
        #[arg(long)]
        year: i32,

        /// Postgres connection URL for the signal/backtest database.
        #[arg(long, env = "DATABASE_URL")]
        database_url: String,
    },
}

#[derive(Subcommand)]
enum CompletionsShell {
    /// Generate zsh completions.
    ///
    /// Install with:
    ///   source <(nexus completions zsh)
    /// or copy to ~/.zsh/completions/_nexus and restart your shell.
    Zsh,
}

const ZSH_COMPLETION_SCRIPT: &str = r#"#compdef nexus

_nexus() {
  local state

  _arguments \
    '(-h --help)'{-h,--help}'[show help]' \
    '1: :->command' \
    '*:: :->args'

  case $state in
    command)
      local commands
      commands=(
        'register:Register tickers for signal tracking'
        'completions:Print shell completion scripts'
      )
      _describe 'command' commands
      ;;
    args)
      case $words[1] in
        completions)
          local shells
          shells=('zsh:Generate zsh completions')
          _describe 'shell' shells
          ;;
      esac
      ;;
  esac
}

_nexus "$@"
"#;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Register {
            tickers,
            exchange_mic,
            brokers,
            topic,
        } => cmd_register(tickers, exchange_mic, brokers, topic).await,

        Commands::Calendar {
            action:
                CalendarAction::Sync {
                    exchange,
                    year,
                    database_url,
                },
        } => cmd_calendar_sync(exchange, year, database_url).await,

        Commands::Completions {
            shell: CompletionsShell::Zsh,
        } => cmd_completions_zsh(),
    }
}

#[derive(Deserialize)]
struct YahooChart {
    chart: YahooChartInner,
}

#[derive(Deserialize)]
struct YahooChartInner {
    result: Option<Vec<serde::de::IgnoredAny>>,
}

/// Returns `true` if Yahoo Finance recognises `ticker` as a valid symbol.
async fn validate_ticker(client: &reqwest::Client, ticker: &str) -> Result<bool> {
    let url = format!("https://query1.finance.yahoo.com/v8/finance/chart/{ticker}");
    let resp = client
        .get(&url)
        .header("User-Agent", "Mozilla/5.0")
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .context("Yahoo Finance request failed")?;

    if !resp.status().is_success() {
        return Ok(false);
    }

    let body: YahooChart = resp
        .json()
        .await
        .context("failed to parse Yahoo Finance response")?;
    Ok(body.chart.result.is_some_and(|r| !r.is_empty()))
}

/// Validate each ticker against Yahoo Finance, then publish `TickerRegistration`
/// protobuf messages for valid ones to Kafka.
async fn cmd_register(
    tickers: Vec<String>,
    exchange_mic: String,
    brokers: String,
    topic: String,
) -> Result<()> {
    let http = reqwest::Client::new();

    // Validate all tickers concurrently.
    let validation_futures: Vec<_> = tickers
        .iter()
        .map(|raw| {
            let ticker = raw.to_uppercase();
            let http = http.clone();
            async move {
                match validate_ticker(&http, &ticker).await {
                    Ok(true) => Ok(Some(ticker)),
                    Ok(false) => {
                        eprintln!("✗ {ticker} — not found on Yahoo Finance");
                        Ok(None)
                    }
                    Err(e) => Err(e.context(format!("{ticker}: Yahoo Finance validation failed"))),
                }
            }
        })
        .collect();

    let mut validated: Vec<String> = Vec::new();
    let mut had_invalid = false;
    for res in join_all(validation_futures).await {
        match res? {
            Some(ticker) => validated.push(ticker),
            None => had_invalid = true,
        }
    }

    if validated.is_empty() {
        std::process::exit(1);
    }

    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &brokers)
        .set("message.timeout.ms", "5000")
        .create()
        .context("failed to create Kafka producer")?;

    let publish_futures: Vec<_> = validated
        .into_iter()
        .map(|ticker| {
            let topic = topic.clone();
            let producer = producer.clone();
            let exchange_mic = exchange_mic.clone();

            async move {
                let payload = TickerRegistration {
                    ticker: ticker.clone(),
                    exchange_mic: exchange_mic.clone(),
                }
                .encode_to_vec();

                let record = FutureRecord::to(&topic)
                    .key(ticker.as_str())
                    .payload(payload.as_slice());

                match producer.send(record, Duration::from_secs(5)).await {
                    Ok(_) => {
                        println!("✓ {ticker} — registered");
                        true
                    }
                    Err((e, _)) => {
                        eprintln!("✗ {ticker} — delivery failed: {e}");
                        false
                    }
                }
            }
        })
        .collect();

    let results = join_all(publish_futures).await;

    if had_invalid || results.into_iter().any(|ok| !ok) {
        std::process::exit(1);
    }

    Ok(())
}

/// Print the zsh completion script to stdout.
fn cmd_completions_zsh() -> Result<()> {
    print!("{ZSH_COMPLETION_SCRIPT}");
    Ok(())
}

// ── Nager.Date API types ──────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NagerHoliday {
    date: NaiveDate,
    name: String,
}

// ── FNSE holiday mapping ──────────────────────────────────────────────────

/// Swedish public holidays that Nasdaq Stockholm / First North (FNSE) does
/// NOT close for. Everything else from the SE Nager feed is treated as a
/// full closure (unless remapped to half_day below).
const FNSE_SKIP: &[&str] = &[
    "Epiphany",        // Jan 6 — not observed by Nasdaq Stockholm
    "Easter Sunday",   // always a Sunday, no exchange effect
    "Pentecost",       // always a Sunday, no exchange effect
    "Whit Sunday",     // alternate name
    "Midsummer Day",   // the Saturday — Midsummer Eve (Friday) is the closure
    "All Saints' Day", // Nov 1 — not observed by Nasdaq Stockholm
];

/// Holidays that Nasdaq Stockholm observes as an early-close (half day)
/// rather than a full closure.
const FNSE_HALF_DAY: &[&str] = &[
    "Christmas Eve",  // Dec 24 — early close 13:00 CET
    "New Year's Eve", // Dec 31 — early close 13:00 CET
];

/// Fetch trading holidays for `year` from Nager.Date, apply the FNSE
/// mapping, and upsert the results into `trading_holidays`.
async fn cmd_calendar_sync(exchange: String, year: i32, database_url: String) -> Result<()> {
    if exchange != "FNSE" {
        anyhow::bail!(
            "only FNSE is currently supported; got {exchange}. \
             Add a mapping in cmd_calendar_sync to support other exchanges."
        );
    }

    // Fetch from Nager.Date (free, no key required).
    let url = format!("https://date.nager.at/api/v3/publicholidays/{year}/SE");
    let client = reqwest::Client::new();
    let holidays: Vec<NagerHoliday> = client
        .get(&url)
        .header("User-Agent", "nexus-cli")
        .send()
        .await
        .context("failed to contact Nager.Date API")?
        .error_for_status()
        .context("Nager.Date API returned an error")?
        .json()
        .await
        .context("failed to parse Nager.Date response")?;

    // Map to (date, status, note).
    let mut entries: Vec<(NaiveDate, &str, &str)> = Vec::new();

    for h in &holidays {
        let wd = h.date.weekday();
        // Skip weekends — they're already non-trading days.
        if wd == Weekday::Sat || wd == Weekday::Sun {
            continue;
        }
        // Skip holidays Nasdaq Stockholm doesn't observe.
        if FNSE_SKIP.iter().any(|s| h.name.contains(s)) {
            continue;
        }

        let status = if FNSE_HALF_DAY.iter().any(|s| h.name.contains(s)) {
            "half_day"
        } else {
            "closed"
        };

        entries.push((h.date, status, &h.name));
    }

    if entries.is_empty() {
        println!("No entries to upsert for {exchange} {year} — check the Nager.Date response.");
        return Ok(());
    }

    // Connect to DB and upsert.
    let pool = sqlx::PgPool::connect(&database_url)
        .await
        .context("failed to connect to database")?;

    let mut upserted = 0usize;
    for (date, status, note) in &entries {
        sqlx::query(
            "INSERT INTO trading_holidays (exchange_mic, date, status, note)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (exchange_mic, date) DO UPDATE
               SET status = EXCLUDED.status,
                   note   = EXCLUDED.note",
        )
        .bind(&exchange)
        .bind(date)
        .bind(status)
        .bind(note)
        .execute(&pool)
        .await
        .with_context(|| format!("failed to upsert {date}"))?;

        println!("  {date}  {status:<8}  {note}");
        upserted += 1;
    }

    println!("\n✓ {upserted} entries upserted for {exchange} {year}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use model::generated::TickerRegistration;
    use prost::Message;

    #[test]
    fn ticker_registration_round_trip() {
        let original = TickerRegistration {
            ticker: "aapl".to_string(),
            exchange_mic: "XNAS".to_string(),
        };
        let encoded = original.encode_to_vec();
        let decoded = TickerRegistration::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.ticker.to_uppercase(), "AAPL");
    }
}
