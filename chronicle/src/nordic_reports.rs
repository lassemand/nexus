/// Nordic report-release publisher (NEX-160, NEX-166, NEX-171).
///
/// For every Nordic company (`NORDIC_MICS`: `FNSE`, `XSTO`, `XCSE`, `FNDK`, `XHEL`, `FNFI`) with a
/// `companies.mfn_slug`, pages through MFN's JSON news feed
/// (`https://mfn.se/all/a/<slug>.json`, fetched despite `robots.txt` with the
/// owner's approval), classifies each item and publishes reports and
/// corrections as `SpecialEvent`s on `special.events`; signal stores them.
/// chronicle keeps only its read position (`mfn_ingest_state`).
///
/// Incremental from the feed cursor by default; `--backfill` pages to the end
/// of each feed. Companies that announced a calendar change get their MFN
/// calendar refreshed at the end of the run. Results releases that
/// `nordic-live` did not post to Slack are posted late (NEX-223; never on
/// `--backfill`). Exits non-zero if any company failed, never because of Slack.
use chronicle_lib::nordic::calendar::{MfnClient, MFN_BASE_URL};
use chronicle_lib::nordic::db;
use chronicle_lib::nordic::events::{KafkaSink, SPECIAL_EVENTS_TOPIC};
use chronicle_lib::nordic::ingest::{process_companies_with, process_insiders_only};
use chronicle_lib::nordic::insiders::{mar_start, KafkaInsiderSink, INSIDER_TRANSACTIONS_TOPIC};
use chronicle_lib::nordic::poll_slack::PollSlack;
use chronicle_lib::nordic::slack::{SlackClient, DEFAULT_API_URL};
use chronicle_lib::nordic::NORDIC_MICS;
use chronicle_lib::registry;
use clap::Parser;
use sqlx::postgres::PgPoolOptions;
use std::process::ExitCode;
use std::time::Duration;
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(about = "Publish Nordic report releases from MFN's news feed as special events")]
struct Args {
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    #[arg(long, env = "MFN_BASE_URL", default_value = MFN_BASE_URL)]
    mfn_base_url: String,

    /// Page every company's feed to the end, ignoring the stored cursor.
    #[arg(long, env = "NORDIC_BACKFILL")]
    backfill: bool,

    #[arg(long, env = "KAFKA_BROKERS")]
    kafka_brokers: String,

    #[arg(long, env = "KAFKA_TICKERS_TOPIC", default_value = registry::TICKERS_TOPIC)]
    tickers_topic: String,

    /// Topic signal consumes special events from.
    #[arg(long, env = "SPECIAL_EVENTS_TOPIC", default_value = SPECIAL_EVENTS_TOPIC)]
    special_events_topic: String,

    /// Topic signal consumes insider transactions from.
    #[arg(long, env = "INSIDER_TRANSACTIONS_TOPIC", default_value = INSIDER_TRANSACTIONS_TOPIC)]
    insider_transactions_topic: String,

    /// Only re-read Finnish companies' feeds for managers' transactions and
    /// publish those. No report events, and no cursor is read or moved. Use it
    /// for history (`--from`) or to re-send; it is idempotent.
    #[arg(long, env = "NORDIC_INSIDERS_ONLY")]
    insiders_only: bool,

    /// With `--insiders-only`: the oldest release to read (YYYY-MM-DD, UTC).
    /// MAR started on 2016-07-03; earlier releases are ignored anyway.
    #[arg(long, env = "NORDIC_INSIDERS_FROM")]
    from: Option<chrono::NaiveDate>,

    /// Only this ticker (with `--insiders-only`).
    #[arg(long, env = "NORDIC_TICKER")]
    ticker: Option<String>,

    /// Only this MIC (with `--insiders-only`), e.g. `XHEL`.
    #[arg(long, env = "NORDIC_MIC")]
    mic: Option<String>,

    /// Slack bot token (`chat:write`). Without it, or without `SLACK_CHANNEL`,
    /// nothing is posted to Slack.
    #[arg(long, env = "SLACK_BOT_TOKEN", hide_env_values = true)]
    slack_bot_token: Option<String>,

    /// Slack channel ID, the same as `nordic-live`'s.
    #[arg(long, env = "SLACK_CHANNEL")]
    slack_channel: Option<String>,

    /// Slack Web API base URL.
    #[arg(long, env = "SLACK_API_URL", default_value = DEFAULT_API_URL)]
    slack_api_url: String,
}

/// The Slack fallback, or `None` when it is off: `--backfill` never posts, and
/// a missing token or channel (or a client that cannot start) disables it.
fn slack_fallback(args: &Args, pool: &sqlx::PgPool) -> Option<PollSlack> {
    if args.backfill {
        return None;
    }
    let (Some(token), Some(channel)) = (&args.slack_bot_token, &args.slack_channel) else {
        warn!("Slack disabled: SLACK_BOT_TOKEN or SLACK_CHANNEL is not set");
        return None;
    };
    if token.is_empty() || channel.is_empty() {
        warn!("Slack disabled: SLACK_BOT_TOKEN or SLACK_CHANNEL is empty");
        return None;
    }
    match SlackClient::with_api_url(token, channel, &args.slack_api_url) {
        Ok(client) => Some(PollSlack::new(pool.clone(), Box::new(client))),
        Err(e) => {
            warn!(error = %e, "Slack disabled: the client could not start");
            None
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/.env")).ok();
    tracing_subscriber::fmt::init();
    match run(Args::parse()).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            error!(error = %e, "nordic-reports failed");
            ExitCode::FAILURE
        }
    }
}

/// Returns `Ok(false)` if any attempted company failed.
async fn run(args: Args) -> anyhow::Result<bool> {
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&args.database_url)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    registry::sync_or_warn(&pool, Some(&args.kafka_brokers), &args.tickers_topic).await;

    let sink = KafkaSink::new(&args.kafka_brokers, &args.special_events_topic)?;
    let insider_sink =
        KafkaInsiderSink::new(&args.kafka_brokers, &args.insider_transactions_topic)?;
    let mut companies = db::load_companies(&pool, NORDIC_MICS).await?;
    let mut feed = MfnClient::new(args.mfn_base_url.clone(), Duration::from_secs(1))?;

    if args.insiders_only {
        companies.retain(|c| {
            args.ticker
                .as_deref()
                .is_none_or(|t| c.ticker.eq_ignore_ascii_case(t))
                && args
                    .mic
                    .as_deref()
                    .is_none_or(|m| c.exchange_mic.eq_ignore_ascii_case(m))
        });
        let from = args.from.unwrap_or_else(mar_start);
        let from = from.and_hms_opt(0, 0, 0).expect("midnight").and_utc();
        let summary = process_insiders_only(&mut feed, &insider_sink, &companies, from).await;
        info!(
            attempted = summary.attempted,
            failed = summary.failed,
            insiders_published = summary.insiders_published,
            mar_parse_errors = summary.mar_parse_errors,
            mar_suspicious_zero_price = summary.mar_suspicious_zero_price,
            "nordic-reports --insiders-only done"
        );
        return Ok(summary.is_success());
    }
    let mut calendar = MfnClient::new(args.mfn_base_url.clone(), Duration::from_secs(2))?;
    let slack = slack_fallback(&args, &pool);

    let summary = process_companies_with(
        &pool,
        &mut feed,
        &mut calendar,
        &sink,
        &insider_sink,
        &companies,
        args.backfill,
        slack.as_ref(),
    )
    .await;
    if summary.slack.skipped_old > 0 {
        info!(
            skipped_old = summary.slack.skipped_old,
            "results releases older than 48 hours were not posted to Slack"
        );
    }
    if summary.slack.failed > 0 {
        error!(
            slack_failed = summary.slack.failed,
            "some Slack posts failed; the run itself is unaffected"
        );
    }
    info!(
        attempted = summary.attempted,
        failed = summary.failed,
        insiders_published = summary.insiders_published,
        mar_parse_errors = summary.mar_parse_errors,
        mar_suspicious_zero_price = summary.mar_suspicious_zero_price,
        calendars_refreshed = ?summary.calendars_refreshed,
        slack_posted = summary.slack.posted,
        slack_updated = summary.slack.updated,
        slack_failed = summary.slack.failed,
        slack_skipped_old = summary.slack.skipped_old,
        "nordic-reports done"
    );
    Ok(summary.is_success())
}
