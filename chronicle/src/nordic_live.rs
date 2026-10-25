/// Live MFN listener (NEX-170): logs Nordic releases the moment they are published.
///
/// Connects to MFN's unofficial WebSocket (`wss://mfn.se/all/s/nordic.json`),
/// picks out items from companies registered with an `mfn_slug`, classifies them
/// with the same rules as the hourly `nordic-reports` poll and logs one
/// structured event per new item (`event = "report_released"` for results
/// releases, `"company_news"` otherwise). It writes nothing: the poll stays the
/// only writer and is the fallback for anything missed while disconnected. See
/// `chronicle_lib::nordic::live`.
///
/// Exits non-zero only on a startup error (bad config, database unreachable at
/// start). Network trouble never ends the process.
use chronicle_lib::nordic::live::{
    handle_message, Companies, Counters, IngestedCheck, Listener, Registered, SeenSet, SignalDb,
    DEFAULT_WS_URL, SEEN_CAPACITY,
};
use clap::Parser;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Row};
use std::process::ExitCode;
use std::str::FromStr;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

const RELOAD_EVERY: Duration = Duration::from_secs(10 * 60);
const STATS_EVERY: Duration = Duration::from_secs(10 * 60);

#[derive(Parser)]
#[command(about = "Log Nordic releases live from MFN's WebSocket")]
struct Args {
    /// MFN WebSocket (JSON, not the HTML-fragment variant without `.json`).
    #[arg(long, env = "MFN_WS_URL", default_value = DEFAULT_WS_URL)]
    ws_url: String,

    /// Chronicle's database: the registered companies (`companies.mfn_slug`).
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    /// Signal's database, read-only, for the poll's "already ingested" check.
    /// Without it every relevant item is logged as new.
    #[arg(long, env = "SIGNAL_DATABASE_URL")]
    signal_database_url: Option<String>,

    /// `json` (default, one JSON object per line) or `text`.
    #[arg(long, env = "LOG_FORMAT", default_value = "json")]
    log_format: String,
}

async fn load_companies(pool: &PgPool) -> sqlx::Result<Companies> {
    let rows = sqlx::query(
        "SELECT ticker, exchange_mic, mfn_slug FROM companies WHERE mfn_slug IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;
    Ok(Companies::new(
        rows.iter()
            .map(|r| Registered {
                ticker: r.get("ticker"),
                exchange_mic: r.get("exchange_mic"),
                mfn_slug: r.get("mfn_slug"),
            })
            .collect(),
    ))
}

#[tokio::main]
async fn main() -> ExitCode {
    dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/.env")).ok();
    let args = Args::parse();
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    if args.log_format == "text" {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    } else {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .init();
    }

    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!(error = %e, "nordic-live failed to start");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> anyhow::Result<()> {
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&args.database_url)
        .await?;
    let mut companies = load_companies(&pool).await?;
    info!(companies = companies.len(), url = %args.ws_url, "nordic-live starting");

    // Read-only and lazy: the live path must not depend on this database being up.
    let signal: Option<SignalDb> = match &args.signal_database_url {
        Some(url) => {
            let options =
                PgConnectOptions::from_str(url)?.options([("default_transaction_read_only", "on")]);
            let pool = PgPoolOptions::new()
                .max_connections(2)
                .acquire_timeout(Duration::from_secs(3))
                .connect_lazy_with(options);
            Some(SignalDb(pool))
        }
        None => {
            warn!("SIGNAL_DATABASE_URL not set: the poll's already-ingested check is off");
            None
        }
    };

    let (tx, mut rx) = mpsc::channel(256);
    let listener = Listener::new(
        args.ws_url.clone(),
        "nexus-chronicle/nordic-live (lasse.alm@gsfleet.io)",
    );
    tokio::spawn(async move { listener.run(tx).await });

    let mut seen = SeenSet::new(SEEN_CAPACITY);
    let mut window = Counters::default();
    let mut reload = tokio::time::interval(RELOAD_EVERY);
    let mut stats = tokio::time::interval(STATS_EVERY);
    reload.tick().await;
    stats.tick().await;
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    loop {
        tokio::select! {
            received = rx.recv() => {
                let Some(msg) = received else { return Ok(()) };
                let check = signal.as_ref().map(|s| s as &dyn IngestedCheck);
                let (counters, _) =
                    handle_message(&msg.text, msg.received_at, &companies, &mut seen, check).await;
                window.seen += counters.seen;
                window.relevant += counters.relevant;
            }
            _ = reload.tick() => match load_companies(&pool).await {
                Ok(c) => {
                    if c.len() != companies.len() {
                        info!(old = companies.len(), new = c.len(), "registered companies changed");
                    }
                    companies = c;
                }
                Err(e) => warn!(error = %e, "reloading companies failed, keeping the old list"),
            },
            _ = stats.tick() => {
                window.log();
                window = Counters::default();
            }
            _ = tokio::signal::ctrl_c() => { info!("interrupted, shutting down"); return Ok(()); }
            _ = term.recv() => { info!("terminated, shutting down"); return Ok(()); }
        }
    }
}
