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
        /// Ticker symbols to register (e.g. AAPL MSFT GOOG).
        #[arg(required = true)]
        tickers: Vec<String>,

        /// Kafka broker addresses.
        #[arg(long, env = "KAFKA_BROKERS", default_value = "localhost:9092")]
        brokers: String,

        /// Kafka topic for ticker registrations.
        #[arg(long, env = "KAFKA_TICKERS_TOPIC", default_value = "company.tickers")]
        topic: String,
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
async fn cmd_register(tickers: Vec<String>, brokers: String, topic: String) -> Result<()> {
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

            async move {
                let payload = TickerRegistration {
                    ticker: ticker.clone(),
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

#[cfg(test)]
mod tests {
    use model::generated::TickerRegistration;
    use prost::Message;

    #[test]
    fn ticker_registration_round_trip() {
        let original = TickerRegistration {
            ticker: "aapl".to_string(),
        };
        let encoded = original.encode_to_vec();
        let decoded = TickerRegistration::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.ticker.to_uppercase(), "AAPL");
    }
}
