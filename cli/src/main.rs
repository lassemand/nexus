mod lookup;
mod saxo_auth;

use alpha::CalendarProvider;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use futures::future::join_all;
use model::generated::{ReportNote, TickerRegistration};
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
    /// For Nordic listings it also looks up the MFN company (slug and name)
    /// and, for Swedish ones, the FI insider-register issuer, and asks when a
    /// lookup has several candidates. Registering an already registered
    /// ticker again fills in its mappings.
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

        /// Override the MIC Yahoo implies, e.g. `FNDK` for First North Denmark
        /// or `XSTO` for a Stockholm main-market share. Must be a known MIC in
        /// the same country as Yahoo's exchange.
        #[arg(long)]
        mic: Option<String>,

        /// MFN slug (`https://mfn.se/a/<slug>`); skips the MFN lookup. One ticker only.
        #[arg(long)]
        mfn_slug: Option<String>,

        /// Issuer name in FI's insider register; skips the FI lookup. One ticker only.
        #[arg(long)]
        fi_issuer: Option<String>,

        /// Kafka broker addresses.
        #[arg(long, env = "KAFKA_BROKERS", default_value = "localhost:9092")]
        brokers: String,

        /// Kafka topic for ticker registrations.
        #[arg(long, env = "KAFKA_TICKERS_TOPIC", default_value = "company.tickers")]
        topic: String,
    },

    /// Withdraw the registration of one or more tickers.
    ///
    /// Publishes a TickerRegistration with `unregister` set to the same topic.
    /// chronicle and signal then drop the listing from their `companies`
    /// tables and stop fetching it; data already stored for it is kept.
    /// Registering the ticker again brings it back.
    Unregister {
        /// Ticker symbols to unregister, as given to `register` (e.g. TRMD GOMX.ST).
        #[arg(required = true)]
        tickers: Vec<String>,

        /// The MIC the listing was registered on. Skips the Yahoo Finance
        /// lookup, so it also works for delisted tickers.
        #[arg(long)]
        mic: Option<String>,

        /// Kafka broker addresses.
        #[arg(long, env = "KAFKA_BROKERS", default_value = "localhost:9092")]
        brokers: String,

        /// Kafka topic for ticker registrations.
        #[arg(long, env = "KAFKA_TICKERS_TOPIC", default_value = "company.tickers")]
        topic: String,
    },

    /// Add a note the analyst agent must take into account for a company's report.
    ///
    /// A note made before the report's kernel is prepared is included in the
    /// preparation; a note that arrives after it triggers a new preparation.
    /// The analyst ignores notes for companies that are not registered.
    ///
    /// Example: nexus expect GOMX.ST --period 2026-Q4 "Order intake above SEK 1.2 bn matters"
    Expect {
        /// Ticker symbol, as given to `register` (e.g. GOMX or GOMX.ST).
        ticker: String,

        /// The MIC the listing is on. Skips the Yahoo Finance lookup.
        #[arg(long)]
        mic: Option<String>,

        /// Fiscal period the note is about, e.g. `2026-Q4`, `2026-H1` or `2026`.
        /// Without it the note is about the company's next report.
        #[arg(long, value_parser = parse_period)]
        period: Option<String>,

        /// Author of the note. Defaults to the OS user name.
        #[arg(long)]
        author: Option<String>,

        /// The note, at most 4 KiB. `-` reads it from stdin.
        text: String,

        /// Kafka broker addresses.
        #[arg(long, env = "KAFKA_BROKERS", default_value = "localhost:9092")]
        brokers: String,

        /// Kafka topic for report notes.
        #[arg(long, env = "ANALYST_NOTES_TOPIC", default_value = "analyst.notes")]
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

    /// Saxo Bank OAuth2 operations.
    ///
    /// Example: nexus saxo auth --client-id <ID> --redirect-uri http://localhost:7878/callback \
    ///   --register-endpoint http://localhost:8080/tokens
    #[command(subcommand_required = true)]
    Saxo {
        #[command(subcommand)]
        action: SaxoAction,
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
enum SaxoAction {
    /// Perform the Saxo OAuth2 authorization-code flow and register the
    /// resulting tokens with a running saxo_stream instance.
    ///
    /// Flow:
    ///   1. Opens the Saxo /authorize URL in your browser (or prints it for
    ///      copy-paste if auto-open fails).
    ///   2. Listens on --callback-port for the browser redirect.
    ///   3. Exchanges the authorization code for an access + refresh token pair.
    ///   4. POSTs the tokens to --register-endpoint, which unblocks saxo_stream
    ///      startup — no manual copy-paste into Vault or .env needed.
    ///   5. Prints all four token values to stdout as a fallback record.
    ///
    /// In the normal case (step 4 succeeds), the stdout output can be ignored —
    /// saxo_stream will start streaming automatically once the POST is received.
    Auth {
        /// Saxo application OAuth2 client ID (from the Saxo developer portal).
        #[arg(long, env = "SAXO_CLIENT_ID")]
        client_id: String,

        /// Saxo application OAuth2 client secret.
        ///
        /// Prefer SAXO_CLIENT_SECRET env var over a bare flag value —
        /// CLI arguments are visible in `ps aux` and shell history.
        #[arg(long, env = "SAXO_CLIENT_SECRET")]
        client_secret: String,

        /// Saxo authorization server base URL.
        /// Use the SIM host (default) for testing, the Live host for production.
        #[arg(
            long,
            env = "SAXO_AUTH_BASE",
            default_value = "https://sim.logonvalidation.net"
        )]
        auth_base: String,

        /// Redirect URI registered in the Saxo developer portal.
        /// Must match exactly (scheme, host, port, path).
        /// Example: http://localhost:7878/callback
        #[arg(long, env = "SAXO_REDIRECT_URI")]
        redirect_uri: String,

        /// Local port to listen on for the OAuth callback redirect.
        /// Must match the port in --redirect-uri.
        #[arg(
            long,
            env = "SAXO_CALLBACK_PORT",
            default_value_t = saxo_auth::DEFAULT_CALLBACK_PORT
        )]
        callback_port: u16,

        /// saxo_stream /tokens endpoint to POST the token pair to.
        /// Receiving this POST unblocks saxo_stream's startup and starts streaming.
        /// Reach it via kubectl port-forward:
        ///   kubectl port-forward deploy/saxo-stream 8080:8081
        /// then pass: http://localhost:8080/tokens
        #[arg(long, env = "SAXO_REGISTER_ENDPOINT")]
        register_endpoint: String,
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
    ///   nexus calendar sync --country DK --from-year 2015 --to-year 2027
    Sync {
        /// ISO 3166-1 alpha-2 country code (e.g. SE, US, DK).
        #[arg(long)]
        country: String,

        /// Calendar year to fetch (e.g. 2028).
        #[arg(long, conflicts_with_all = ["from_year", "to_year"], required_unless_present = "from_year")]
        year: Option<i32>,

        /// First year of a range to fetch (inclusive).
        #[arg(long, requires = "to_year")]
        from_year: Option<i32>,

        /// Last year of a range to fetch (inclusive).
        #[arg(long, requires = "from_year")]
        to_year: Option<i32>,
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
        'unregister:Withdraw ticker registrations'
        'expect:Add a note for the analyst about a company report'
        'calendar:Fetch and store trading holiday data for an exchange'
        'saxo:Saxo Bank OAuth2 operations'
        'completions:Print shell completion scripts'
      )
      _describe 'command' commands
      ;;
    args)
      case $words[1] in
        calendar)
          local actions
          actions=('sync:Fetch trading holidays and upsert into the DB')
          _describe 'action' actions
          ;;
        saxo)
          local actions
          actions=('auth:Perform the Saxo OAuth2 authorization-code flow')
          _describe 'action' actions
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
            mic,
            mfn_slug,
            fi_issuer,
            brokers,
            topic,
        } => {
            let overrides = lookup::Overrides {
                mfn_slug,
                fi_issuer,
            };
            cmd_register(tickers, mic, overrides, brokers, topic).await
        }

        Commands::Unregister {
            tickers,
            mic,
            brokers,
            topic,
        } => cmd_unregister(tickers, mic, brokers, topic).await,

        Commands::Expect {
            ticker,
            mic,
            period,
            author,
            text,
            brokers,
            topic,
        } => {
            let text = if text == "-" {
                let mut buf = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
                    .context("failed to read the note from stdin")?;
                buf
            } else {
                text
            };
            let text = match validate_note_text(&text) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("✗ {e}");
                    std::process::exit(2);
                }
            };
            cmd_expect(ticker, mic, period, author, text, brokers, topic).await
        }

        Commands::Calendar {
            action:
                CalendarAction::Sync {
                    country,
                    year,
                    from_year,
                    to_year,
                },
        } => {
            let (from, to) = match (year, from_year, to_year) {
                (Some(y), _, _) => (y, y),
                (None, Some(f), Some(t)) => (f, t),
                // clap guarantees one of the two forms
                _ => anyhow::bail!("pass --year, or --from-year with --to-year"),
            };
            for y in from..=to {
                cmd_calendar_sync(country.clone(), y).await?;
            }
            Ok(())
        }

        Commands::Saxo {
            action:
                SaxoAction::Auth {
                    client_id,
                    client_secret,
                    auth_base,
                    redirect_uri,
                    callback_port,
                    register_endpoint,
                },
        } => {
            saxo_auth::cmd_saxo_auth(
                &client_id,
                &client_secret,
                &auth_base,
                &redirect_uri,
                callback_port,
                &register_endpoint,
            )
            .await
        }

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

/// Suffix-stripped, uppercase ticker (`NOVO-B.CO` → `NOVO-B`).
fn canonical_ticker(input: &str) -> String {
    input.split('.').next().unwrap_or(input).to_uppercase()
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
    mic_override: Option<&str>,
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
                 mapping table — add it to a model::nordic venue (Nordic) or model::asset::mic::YAHOO_EXCHANGE_TO_MIC \
                 before registering this ticker"
            );
        }
    };

    let exchange_mic = mic::apply_override(&exchange_mic, mic_override)
        .map_err(|e| anyhow::anyhow!("{input}: {e}"))?;

    let currency = match result.meta.currency.filter(|c| !c.is_empty()) {
        Some(c) => c,
        None => mic::currency(&exchange_mic)
            .with_context(|| {
                format!("Yahoo gave no currency for '{input}' and MIC {exchange_mic} has no known currency")
            })?
            .to_string(),
    };

    // Strip Yahoo exchange suffix (e.g. ".ST") to get the canonical ticker.
    let canonical = canonical_ticker(input);

    Ok(Some(ResolvedTicker {
        canonical,
        exchange_mic,
        currency,
    }))
}

/// Validate each ticker against Yahoo Finance (resolving exchange + currency
/// automatically), then publish `TickerRegistration` protobuf messages to Kafka.
async fn cmd_register(
    tickers: Vec<String>,
    mic_override: Option<String>,
    overrides: lookup::Overrides,
    brokers: String,
    topic: String,
) -> Result<()> {
    if tickers.len() > 1 && (overrides.mfn_slug.is_some() || overrides.fi_issuer.is_some()) {
        anyhow::bail!("--mfn-slug and --fi-issuer apply to one ticker at a time");
    }
    let http = reqwest::Client::new();

    // Resolve all tickers concurrently.
    let resolution_futures: Vec<_> = tickers
        .iter()
        .map(|raw| {
            let input = raw.clone();
            let http = http.clone();
            let mic_override = mic_override.clone();
            async move {
                match validate_and_resolve(&http, &input, mic_override.as_deref()).await {
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

    // One ticker at a time, so a question is never interleaved with another's.
    let lookups = lookup::Lookup::new(http);
    let mut chooser = lookup::TerminalChooser;
    let mut messages = Vec::with_capacity(validated.len());
    for resolved in validated {
        let m = lookups
            .mappings(
                &resolved.canonical,
                &resolved.exchange_mic,
                &overrides,
                &mut chooser,
            )
            .await;
        messages.push(TickerRegistration {
            ticker: resolved.canonical,
            exchange_mic: resolved.exchange_mic,
            currency: resolved.currency,
            unregister: false,
            name: m.name.unwrap_or_default(),
            mfn_slug: m.mfn_slug.unwrap_or_default(),
            fi_issuer: m.fi_issuer.unwrap_or_default(),
            isin: m.isin.unwrap_or_default(),
        });
    }
    let delivered = publish(&brokers, &topic, messages).await?;

    if had_invalid || !delivered {
        std::process::exit(1);
    }

    Ok(())
}

/// Ticker and MIC to unregister: with `mic`, taken as given (no lookup, so a
/// delisted ticker works too); without it, resolved like `register` does.
async fn resolve_for_unregister(
    http: &reqwest::Client,
    input: &str,
    mic: Option<&str>,
) -> Result<(String, String)> {
    use model::asset::mic as mics;

    if let Some(m) = mic {
        let m = m.to_uppercase();
        if mics::currency(&m).is_none() {
            anyhow::bail!("unknown MIC '{m}'");
        }
        return Ok((canonical_ticker(input), m));
    }
    match validate_and_resolve(http, input, None).await? {
        Some(r) => Ok((r.canonical, r.exchange_mic)),
        None => anyhow::bail!("not found on Yahoo Finance — pass --mic to unregister it anyway"),
    }
}

/// Publish an unregistration for each ticker, so every reader of the topic
/// drops the listing. Data already stored for it is kept.
async fn cmd_unregister(
    tickers: Vec<String>,
    mic: Option<String>,
    brokers: String,
    topic: String,
) -> Result<()> {
    let http = reqwest::Client::new();
    let resolutions = join_all(
        tickers
            .iter()
            .map(|input| resolve_for_unregister(&http, input, mic.as_deref())),
    )
    .await;

    let mut messages = Vec::new();
    let mut had_invalid = false;
    for (input, res) in tickers.iter().zip(resolutions) {
        match res {
            Ok((ticker, exchange_mic)) => messages.push(TickerRegistration {
                ticker,
                exchange_mic,
                unregister: true,
                ..Default::default()
            }),
            Err(e) => {
                eprintln!("✗ {input} — {e}");
                had_invalid = true;
            }
        }
    }
    if messages.is_empty() {
        std::process::exit(1);
    }

    let delivered = publish(&brokers, &topic, messages).await?;
    if had_invalid || !delivered {
        std::process::exit(1);
    }
    Ok(())
}

/// Maximum size of a note's text in bytes, after trimming.
const MAX_NOTE_BYTES: usize = 4096;

/// Validates a fiscal period: `2026`, `2026-Q1`..`2026-Q4` or `2026-H1`/`2026-H2`.
fn parse_period(input: &str) -> Result<String, String> {
    let (year, rest) = input.split_at_checked(4).unwrap_or((input, ""));
    let valid = year.len() == 4
        && year.bytes().all(|b| b.is_ascii_digit())
        && matches!(rest, "" | "-Q1" | "-Q2" | "-Q3" | "-Q4" | "-H1" | "-H2");
    if valid {
        Ok(input.to_string())
    } else {
        Err(format!(
            "invalid period '{input}': expected e.g. 2026, 2026-Q4 or 2026-H1"
        ))
    }
}

/// Trims a note's text and rejects it when blank or longer than 4 KiB.
fn validate_note_text(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("the note is blank".to_string());
    }
    if text.len() > MAX_NOTE_BYTES {
        return Err(format!(
            "the note is {} bytes; the limit is {MAX_NOTE_BYTES}",
            text.len()
        ));
    }
    Ok(text.to_string())
}

/// The OS user name from the environment, or `unknown`.
fn os_user() -> String {
    ["USER", "USERNAME", "LOGNAME"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| "unknown".to_string())
}

/// Builds the `ReportNote` and its Kafka key `TICKER:MIC`.
fn build_note(
    ticker: String,
    exchange_mic: String,
    period: Option<String>,
    author: Option<String>,
    text: String,
    now_unix_secs: i64,
) -> (String, ReportNote) {
    let key = format!("{ticker}:{exchange_mic}");
    let note = ReportNote {
        ticker,
        exchange_mic,
        fiscal_period: period.unwrap_or_default(),
        text,
        author: author
            .filter(|a| !a.trim().is_empty())
            .unwrap_or_else(os_user),
        created_at_unix_secs: now_unix_secs,
        note_id: uuid::Uuid::new_v4().to_string(),
    };
    (key, note)
}

/// Resolves the listing like `register`, publishes one `ReportNote` and waits
/// for the broker's acknowledgement; exits with code 1 on any failure.
async fn cmd_expect(
    ticker: String,
    mic: Option<String>,
    period: Option<String>,
    author: Option<String>,
    text: String,
    brokers: String,
    topic: String,
) -> Result<()> {
    let http = reqwest::Client::new();
    let (ticker, exchange_mic) = match resolve_for_unregister(&http, &ticker, mic.as_deref()).await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("✗ {ticker} — {e}");
            std::process::exit(1);
        }
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs() as i64;
    let (key, note) = build_note(ticker, exchange_mic, period, author, text, now);

    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &brokers)
        .set("message.timeout.ms", "5000")
        .create()
        .context("failed to create Kafka producer")?;
    let payload = note.encode_to_vec();
    let record = FutureRecord::to(&topic)
        .key(key.as_str())
        .payload(payload.as_slice());
    match producer.send(record, Duration::from_secs(5)).await {
        Ok(_) => {
            println!(
                "✓ Queued note {} for {key}; the analyst ignores notes for companies that aren't registered.",
                note.note_id
            );
            Ok(())
        }
        Err((e, _)) => {
            eprintln!("✗ {key} — delivery failed: {e}");
            std::process::exit(1);
        }
    }
}

/// Publishes each message to `topic`, keyed by its ticker, and prints the
/// outcome per ticker. Returns whether every message was delivered.
async fn publish(brokers: &str, topic: &str, messages: Vec<TickerRegistration>) -> Result<bool> {
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", brokers)
        .set("message.timeout.ms", "5000")
        .create()
        .context("failed to create Kafka producer")?;

    let results = join_all(messages.into_iter().map(|msg| {
        let producer = producer.clone();
        async move {
            let payload = msg.encode_to_vec();
            let record = FutureRecord::to(topic)
                .key(msg.ticker.as_str())
                .payload(payload.as_slice());
            match producer.send(record, Duration::from_secs(5)).await {
                Ok(_) if msg.unregister => {
                    println!("✓ {} — unregistered ({})", msg.ticker, msg.exchange_mic);
                    true
                }
                Ok(_) => {
                    println!(
                        "✓ {} — registered ({} / {}){}",
                        msg.ticker,
                        msg.exchange_mic,
                        msg.currency,
                        mapping_summary(&msg)
                    );
                    true
                }
                Err((e, _)) => {
                    eprintln!("✗ {} — delivery failed: {e}", msg.ticker);
                    false
                }
            }
        }
    }))
    .await;
    Ok(results.into_iter().all(|ok| ok))
}

/// `, MFN torm (Torm), FI …` for the mappings a registration carries.
fn mapping_summary(msg: &TickerRegistration) -> String {
    let mut parts = Vec::new();
    if !msg.mfn_slug.is_empty() {
        parts.push(if msg.name.is_empty() {
            format!("MFN {}", msg.mfn_slug)
        } else {
            format!("MFN {} ({})", msg.mfn_slug, msg.name)
        });
    }
    if !msg.isin.is_empty() {
        parts.push(format!("ISIN {}", msg.isin));
    }
    if !msg.fi_issuer.is_empty() {
        parts.push(format!("FI issuer {}", msg.fi_issuer));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(", {}", parts.join(", "))
    }
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
    use super::*;
    use model::asset::mic;
    use model::generated::TickerRegistration;
    use prost::Message;

    #[test]
    fn period_accepts_year_quarter_and_half() {
        for ok in ["2026", "2026-Q4", "2026-Q1", "2026-H1", "2026-H2"] {
            assert_eq!(parse_period(ok).as_deref(), Ok(ok));
        }
        for bad in [
            "",
            "26",
            "2026-Q5",
            "2026-Q0",
            "2026-H3",
            "2026Q4",
            "2026-q4",
            "20267",
            "2026-",
            "abcd",
            "２０２６",
        ] {
            assert!(parse_period(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn note_text_is_trimmed_and_limited() {
        assert_eq!(validate_note_text("  hi \n").as_deref(), Ok("hi"));
        assert!(validate_note_text("  \n ").is_err());
        assert!(validate_note_text(&"a".repeat(4096)).is_ok());
        assert!(validate_note_text(&"a".repeat(4097)).is_err());
        assert!(validate_note_text(&format!("  {}  ", "a".repeat(4096))).is_ok());
    }

    #[test]
    fn expect_parses_defaults_and_stdin_marker() {
        let cli = Cli::try_parse_from(["nexus", "expect", "GOMX.ST", "-"]).expect("parses");
        match cli.command {
            Commands::Expect {
                ticker,
                mic,
                period,
                author,
                text,
                topic,
                ..
            } => {
                assert_eq!(ticker, "GOMX.ST");
                assert!(mic.is_none() && period.is_none() && author.is_none());
                assert_eq!(text, "-");
                assert_eq!(topic, "analyst.notes");
            }
            _ => panic!("expected the expect command"),
        }
        assert!(
            Cli::try_parse_from(["nexus", "expect", "GOMX", "x", "--period", "2026-Q9"]).is_err()
        );
        assert!(Cli::try_parse_from(["nexus", "expect", "GOMX"]).is_err());
    }

    #[test]
    fn note_defaults_author_and_round_trips_every_field() {
        let (key, note) = build_note(
            "GOMX".into(),
            "FNSE".into(),
            Some("2026-Q4".into()),
            Some("mogens".into()),
            "order intake".into(),
            1_700_000_000,
        );
        assert_eq!(key, "GOMX:FNSE");
        assert_eq!(
            uuid::Uuid::parse_str(&note.note_id)
                .expect("uuid")
                .get_version_num(),
            4
        );
        let decoded = ReportNote::decode(note.encode_to_vec().as_slice()).expect("decodes");
        assert_eq!(decoded, note);
        assert_eq!(decoded.author, "mogens");
        assert_eq!(decoded.fiscal_period, "2026-Q4");
        assert_eq!(decoded.created_at_unix_secs, 1_700_000_000);

        let (_, defaulted) = build_note("A".into(), "XSTO".into(), None, None, "t".into(), 1);
        assert_eq!(defaulted.fiscal_period, "");
        assert!(!defaulted.author.is_empty());
    }

    #[test]
    fn ticker_registration_round_trip() {
        let original = TickerRegistration {
            ticker: "aapl".to_string(),
            exchange_mic: "XNAS".to_string(),
            currency: "USD".to_string(),
            ..Default::default()
        };
        let encoded = original.encode_to_vec();
        let decoded = TickerRegistration::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.ticker.to_uppercase(), "AAPL");
    }

    #[tokio::test]
    async fn unregister_with_mic_skips_the_lookup() {
        let http = reqwest::Client::new();
        let resolved = super::resolve_for_unregister(&http, "trmd", Some("xnas"))
            .await
            .unwrap();
        assert_eq!(resolved, ("TRMD".to_string(), "XNAS".to_string()));
        let suffixed = super::resolve_for_unregister(&http, "GOMX.ST", Some("FNSE"))
            .await
            .unwrap();
        assert_eq!(suffixed.0, "GOMX");
        assert!(super::resolve_for_unregister(&http, "TRMD", Some("XXXX"))
            .await
            .is_err());
    }

    #[test]
    fn novo_b_resolves_to_xcse_dkk_and_fndk_with_override() {
        assert_eq!(super::canonical_ticker("NOVO-B.CO"), "NOVO-B");
        let yahoo = mic::mic_for_yahoo_exchange("CPH").unwrap();
        let plain = mic::apply_override(yahoo, None).unwrap();
        assert_eq!(plain, "XCSE");
        assert_eq!(mic::currency(&plain), Some("DKK"));
        let fndk = mic::apply_override(yahoo, Some("FNDK")).unwrap();
        assert_eq!(fndk, "FNDK");
        assert_eq!(mic::currency(&fndk), Some("DKK"));
    }

    #[test]
    fn registration_summary_shows_mfn_slug_name_and_isin() {
        let msg = TickerRegistration {
            ticker: "METSO".into(),
            exchange_mic: "XHEL".into(),
            currency: "EUR".into(),
            mfn_slug: "metso-2".into(),
            name: "Metso".into(),
            isin: "FI0009014575".into(),
            ..Default::default()
        };
        let s = super::mapping_summary(&msg);
        assert!(s.contains("MFN metso-2 (Metso)"), "{s}");
        assert!(s.contains("ISIN FI0009014575"), "{s}");
        assert!(!s.contains("FI issuer"), "{s}");
    }

    #[test]
    fn metso_he_resolves_to_xhel_eur_and_fnfi_override_is_accepted() {
        assert_eq!(super::canonical_ticker("METSO.HE"), "METSO");
        let yahoo = mic::mic_for_yahoo_exchange("HEL").unwrap();
        let plain = mic::apply_override(yahoo, None).unwrap();
        assert_eq!(plain, "XHEL");
        assert_eq!(mic::currency(&plain), Some("EUR"));
        let fnfi = mic::apply_override(yahoo, Some("FNFI")).unwrap();
        assert_eq!(fnfi, "FNFI");
        assert_eq!(mic::currency(&fnfi), Some("EUR"));
    }

    #[test]
    fn xcse_override_on_a_helsinki_symbol_is_an_error() {
        let yahoo = mic::mic_for_yahoo_exchange("HEL").unwrap();
        assert!(mic::apply_override(yahoo, Some("XCSE")).is_err());
        assert!(mic::apply_override(yahoo, Some("FNDK")).is_err());
    }

    #[test]
    fn fndk_override_on_a_stockholm_symbol_is_an_error() {
        let yahoo = mic::mic_for_yahoo_exchange("STO").unwrap();
        assert!(mic::apply_override(yahoo, Some("FNDK")).is_err());
    }

    #[test]
    fn aapl_yahoo_code_resolves_to_xnas_usd() {
        let exchange_mic = mic::mic_for_yahoo_exchange("NMS").unwrap();
        assert_eq!(exchange_mic, mic::XNAS);
        assert_eq!(mic::currency(exchange_mic), Some("USD"));
    }

    #[test]
    fn gomx_st_yahoo_code_resolves_to_fnse_sek() {
        // Yahoo returns "STO" for Stockholm exchange (GOMX.ST)
        let exchange_mic = mic::mic_for_yahoo_exchange("STO").unwrap();
        assert_eq!(exchange_mic, mic::FNSE);
        assert_eq!(mic::currency(exchange_mic), Some("SEK"));
    }

    #[test]
    fn cph_yahoo_code_resolves_to_xcse_dkk() {
        let exchange_mic = mic::mic_for_yahoo_exchange("CPH").unwrap();
        assert_eq!(exchange_mic, mic::XCSE);
        assert_eq!(mic::currency(exchange_mic), Some("DKK"));
    }

    #[test]
    fn nyse_yahoo_code_resolves_to_xnys() {
        let exchange_mic = mic::mic_for_yahoo_exchange("NYQ").unwrap();
        assert_eq!(exchange_mic, mic::XNYS);
        assert_eq!(mic::currency(exchange_mic), Some("USD"));
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
