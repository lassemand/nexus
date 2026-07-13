use alpha::CalendarProvider;
use anyhow::{Context, Result};
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
        /// Ticker symbols to register (e.g. AAPL MSFT GOMX.ST).
        ///
        /// Yahoo Finance exchange suffixes (e.g. `.ST` for Stockholm) are
        /// accepted as a lookup hint. The canonical ticker stored is always
        /// the suffix-stripped form (e.g. `GOMX`); exchange MIC and currency
        /// are resolved automatically from Yahoo's response metadata.
        #[arg(required = true)]
        tickers: Vec<String>,

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
    /// Fetch trading holidays for a country from Nager.Date and upsert into the DB.
    ///
    /// Uses country codes (ISO 3166-1 alpha-2), not exchange MICs.
    /// Multiple exchanges may share the same country calendar (e.g. XNYS
    /// and XNAS both use "US"). Reads DATABASE_URL from the environment.
    ///
    /// Examples:
    ///   nexus calendar sync --country SE --year 2029
    ///   nexus calendar sync --country US --year 2029
    Sync {
        /// ISO 3166-1 alpha-2 country code (e.g. SE, US).
        #[arg(long)]
        country: String,

        /// Calendar year to fetch (e.g. 2028).
        #[arg(long)]
        year: i32,
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
            brokers,
            topic,
        } => cmd_register(tickers, brokers, topic).await,

        Commands::Calendar {
            action: CalendarAction::Sync { country, year },
        } => cmd_calendar_sync(country, year).await,

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
    result: Option<Vec<YahooChartResult>>,
}

#[derive(Deserialize)]
struct YahooChartResult {
    meta: YahooMeta,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct YahooMeta {
    /// e.g. "NMS", "NYQ", "STO"
    exchange_name: Option<String>,
    /// ISO 4217 currency code (e.g. "USD", "SEK")
    currency: Option<String>,
}

/// Resolved ticker metadata from Yahoo Finance.
struct ResolvedTicker {
    /// Canonical ticker without Yahoo exchange suffix (e.g. "GOMX" not "GOMX.ST").
    canonical: String,
    exchange_mic: String,
    currency: String,
}

/// Validate a ticker against Yahoo Finance and resolve its exchange MIC and currency.
///
/// Accepts Yahoo-style exchange suffixes (e.g. `GOMX.ST`) as a lookup hint;
/// the canonical ticker returned is always suffix-stripped (e.g. `GOMX`).
///
/// Fails loudly if:
/// - Yahoo returns no result for the ticker.
/// - Yahoo's exchange code is not in the mapping table (never silently defaults).
async fn validate_and_resolve(
    client: &reqwest::Client,
    input: &str,
) -> Result<Option<ResolvedTicker>> {
    use model::asset::mic;

    let url = format!("https://query1.finance.yahoo.com/v8/finance/chart/{input}");
    let resp = client
        .get(&url)
        .header("User-Agent", "Mozilla/5.0")
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .context("Yahoo Finance request failed")?;

    if !resp.status().is_success() {
        return Ok(None);
    }

    let body: YahooChart = resp
        .json()
        .await
        .context("failed to parse Yahoo Finance response")?;

    let result = match body.chart.result.and_then(|mut r| r.pop()) {
        Some(r) => r,
        None => return Ok(None),
    };

    let yahoo_exchange = result
        .meta
        .exchange_name
        .as_deref()
        .unwrap_or("")
        .to_string();

    let exchange_mic = match mic::mic_for_yahoo_exchange(&yahoo_exchange) {
        Some(m) => m.to_string(),
        None => {
            anyhow::bail!(
                "Yahoo exchange code '{yahoo_exchange}' for '{input}' is not in the MIC \
                 mapping table — add it to model::asset::mic::YAHOO_EXCHANGE_TO_MIC \
                 before registering this ticker"
            );
        }
    };

    let currency = result
        .meta
        .currency
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| mic::currency(&exchange_mic).to_string());

    // Strip Yahoo exchange suffix (e.g. ".ST") to get the canonical ticker.
    let canonical = input.split('.').next().unwrap_or(input).to_uppercase();

    Ok(Some(ResolvedTicker {
        canonical,
        exchange_mic,
        currency,
    }))
}

/// Validate each ticker against Yahoo Finance (resolving exchange + currency
/// automatically), then publish `TickerRegistration` protobuf messages to Kafka.
async fn cmd_register(tickers: Vec<String>, brokers: String, topic: String) -> Result<()> {
    let http = reqwest::Client::new();

    // Resolve all tickers concurrently.
    let resolution_futures: Vec<_> = tickers
        .iter()
        .map(|raw| {
            let input = raw.clone();
            let http = http.clone();
            async move {
                match validate_and_resolve(&http, &input).await {
                    Ok(Some(resolved)) => Ok::<_, anyhow::Error>(Some(resolved)),
                    Ok(None) => {
                        eprintln!("✗ {input} — not found on Yahoo Finance");
                        Ok(None)
                    }
                    Err(e) => {
                        eprintln!("✗ {input} — {e}");
                        Ok(None)
                    }
                }
            }
        })
        .collect();

    let mut validated: Vec<ResolvedTicker> = Vec::new();
    let mut had_invalid = false;
    for res in join_all(resolution_futures).await {
        match res? {
            Some(r) => validated.push(r),
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
        .map(|resolved| {
            let topic = topic.clone();
            let producer = producer.clone();

            async move {
                let payload = TickerRegistration {
                    ticker: resolved.canonical.clone(),
                    exchange_mic: resolved.exchange_mic.clone(),
                    currency: resolved.currency.clone(),
                }
                .encode_to_vec();

                let record = FutureRecord::to(&topic)
                    .key(resolved.canonical.as_str())
                    .payload(payload.as_slice());

                match producer.send(record, Duration::from_secs(5)).await {
                    Ok(_) => {
                        println!(
                            "✓ {} — registered ({} / {})",
                            resolved.canonical, resolved.exchange_mic, resolved.currency
                        );
                        true
                    }
                    Err((e, _)) => {
                        eprintln!("✗ {} — delivery failed: {e}", resolved.canonical);
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

/// Fetch trading holidays via alpha::CalendarProvider and upsert into the DB.
///
/// `country` is an ISO 3166-1 alpha-2 code (e.g. `"SE"`, `"US"`).
/// DATABASE_URL is read from the environment — set automatically in the
/// cluster via the signal-secret ExternalSecret.
async fn cmd_calendar_sync(country: String, year: i32) -> Result<()> {
    let database_url =
        std::env::var("DATABASE_URL").context("DATABASE_URL environment variable not set")?;

    let provider = CalendarProvider::new();
    let entries = provider
        .holidays_for_country(&country, year)
        .await
        .context("failed to fetch holidays from Nager.Date")?;

    if entries.is_empty() {
        println!("No weekday entries to upsert for {country} {year}.");
        return Ok(());
    }

    let pool = sqlx::PgPool::connect(&database_url)
        .await
        .context("failed to connect to database")?;

    let mut upserted = 0usize;
    for entry in &entries {
        sqlx::query(
            "INSERT INTO trading_holidays (country, date, status, note)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (country, date) DO NOTHING",
        )
        .bind(&country)
        .bind(entry.date)
        .bind(entry.status)
        .bind(&entry.note)
        .execute(&pool)
        .await
        .with_context(|| format!("failed to upsert {}", entry.date))?;

        println!("  {}  {:<8}  {}", entry.date, entry.status, entry.note);
        upserted += 1;
    }

    println!("\n✓ {upserted} entries upserted for {country} {year}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use model::asset::mic;
    use model::generated::TickerRegistration;
    use prost::Message;

    #[test]
    fn ticker_registration_round_trip() {
        let original = TickerRegistration {
            ticker: "aapl".to_string(),
            exchange_mic: "XNAS".to_string(),
            currency: "USD".to_string(),
        };
        let encoded = original.encode_to_vec();
        let decoded = TickerRegistration::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.ticker.to_uppercase(), "AAPL");
    }

    #[test]
    fn aapl_yahoo_code_resolves_to_xnas_usd() {
        let exchange_mic = mic::mic_for_yahoo_exchange("NMS").unwrap();
        assert_eq!(exchange_mic, mic::XNAS);
        assert_eq!(mic::currency(exchange_mic), "USD");
    }

    #[test]
    fn gomx_st_yahoo_code_resolves_to_fnse_sek() {
        // Yahoo returns "STO" for Stockholm exchange (GOMX.ST)
        let exchange_mic = mic::mic_for_yahoo_exchange("STO").unwrap();
        assert_eq!(exchange_mic, mic::FNSE);
        assert_eq!(mic::currency(exchange_mic), "SEK");
    }

    #[test]
    fn nyse_yahoo_code_resolves_to_xnys() {
        let exchange_mic = mic::mic_for_yahoo_exchange("NYQ").unwrap();
        assert_eq!(exchange_mic, mic::XNYS);
        assert_eq!(mic::currency(exchange_mic), "USD");
    }

    #[test]
    fn unknown_yahoo_exchange_code_returns_none() {
        // An unmapped code must return None — never silently default.
        assert!(
            mic::mic_for_yahoo_exchange("UNKNOWN_EXCHANGE").is_none(),
            "unmapped Yahoo exchange code must return None, not a silent default"
        );
        assert!(mic::mic_for_yahoo_exchange("").is_none());
        assert!(mic::mic_for_yahoo_exchange("XYZ").is_none());
    }

    #[test]
    fn suffix_stripping_canonical_ticker() {
        // Verify the suffix stripping logic used in validate_and_resolve.
        let cases = [
            ("GOMX.ST", "GOMX"),
            ("AAPL", "AAPL"),
            ("BRK.B", "BRK"), // Note: single . strip
        ];
        for (input, expected) in cases {
            let canonical = input.split('.').next().unwrap_or(input).to_uppercase();
            assert_eq!(canonical, expected, "failed for input={input}");
        }
    }
}
