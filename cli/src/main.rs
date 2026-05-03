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
    /// Publishes a TickerRegistration protobuf message to the configured
    /// Kafka topic for each supplied ticker symbol. All tickers are attempted
    /// even if earlier ones fail; the process exits with code 1 if any
    /// delivery fails.
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

    /// Query Polygon for ticker completions (used internally by shell completions).
    #[command(name = "_complete", hide = true)]
    Complete {
        /// Ticker prefix to search.
        prefix: String,
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

/// Zsh completion script.
///
/// Delegates dynamic ticker lookup to `nexus _complete <prefix>`, which
/// queries the Polygon ticker search endpoint at tab-press time.
const ZSH_COMPLETION_SCRIPT: &str = r#"#compdef nexus

# Nexus zsh completion script
# Install: source <(nexus completions zsh)
# Or: nexus completions zsh > ~/.zsh/completions/_nexus

_nexus_tickers() {
  local prefix="${words[CURRENT]}"
  local raw tickers descriptions line ticker name

  raw=(${(f)"$(nexus _complete "$prefix" 2>/dev/null)"})

  tickers=()
  descriptions=()
  for line in $raw; do
    ticker="${line%%:*}"
    name="${line#*:}"
    tickers+=("$ticker")
    descriptions+=("$name")
  done

  compadd -d descriptions -a tickers
}

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
        register)
          _arguments '*:ticker:_nexus_tickers'
          ;;
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

        Commands::Complete { prefix } => cmd_complete(prefix),
    }
}

/// Publish `TickerRegistration` protobuf messages for each ticker to Kafka.
///
/// All tickers are published concurrently. If any delivery fails the error
/// is printed to stderr and the process ultimately exits with code 1, but
/// remaining tickers are still attempted.
async fn cmd_register(tickers: Vec<String>, brokers: String, topic: String) -> Result<()> {
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &brokers)
        .set("message.timeout.ms", "5000")
        .create()
        .context("failed to create Kafka producer")?;

    let futures: Vec<_> = tickers
        .into_iter()
        .map(|raw| {
            let ticker = raw.to_uppercase();
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
                        println!("✓ {ticker}");
                        true
                    }
                    Err((e, _)) => {
                        eprintln!("✗ {ticker}: {e}");
                        false
                    }
                }
            }
        })
        .collect();

    let results = join_all(futures).await;

    if results.into_iter().any(|ok| !ok) {
        std::process::exit(1);
    }

    Ok(())
}

/// Print the zsh completion script to stdout.
fn cmd_completions_zsh() -> Result<()> {
    print!("{ZSH_COMPLETION_SCRIPT}");
    Ok(())
}

/// Polygon ticker search response shapes.
#[derive(Deserialize)]
struct PolygonResponse {
    results: Option<Vec<PolygonTicker>>,
}

#[derive(Deserialize)]
struct PolygonTicker {
    ticker: String,
    name: String,
}

/// Query Polygon for tickers matching `prefix` and print one `TICKER:Company Name`
/// line per result to stdout.
///
/// Reads `POLYGON_API_KEY` from the environment. If the variable is absent the
/// subcommand exits silently with code 0 so that shell completions degrade
/// gracefully rather than producing error noise.
fn cmd_complete(prefix: String) -> Result<()> {
    let api_key = match std::env::var("POLYGON_API_KEY") {
        Ok(k) => k,
        Err(_) => return Ok(()), // degrade gracefully — no key, no completions
    };

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(800))
        .build()
        .context("failed to build HTTP client")?;

    let response: PolygonResponse = client
        .get("https://api.polygon.io/v3/reference/tickers")
        .query(&[
            ("search", prefix.as_str()),
            ("active", "true"),
            ("limit", "20"),
            ("apiKey", api_key.as_str()),
        ])
        .send()
        .context("Polygon request failed")?
        .json()
        .context("failed to parse Polygon response")?;

    for item in response.results.unwrap_or_default() {
        println!("{}:{}", item.ticker, item.name);
    }

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
