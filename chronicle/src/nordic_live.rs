/// Live MFN listener (NEX-170): logs Nordic releases the moment they are published.
///
/// Connects to MFN's unofficial WebSocket (`wss://mfn.se/all/s/nordic.json`),
/// picks out items from companies registered with an `mfn_slug`, classifies them
/// with the same rules as the hourly `nordic-reports` poll and logs one
/// structured event per new item (`event = "report_released"` for results
/// releases, `"company_news"` otherwise). Results releases are also posted to
/// Slack when `SLACK_BOT_TOKEN` and `SLACK_CHANNEL` are set (NEX-222); the only
/// thing it writes is the `slack_report_posts` claim. The poll stays the
/// fallback for anything missed while disconnected. See `chronicle_lib::nordic::live`
/// and `chronicle_lib::nordic::live_slack`.
///
/// Exits non-zero only on a startup error (bad config, database unreachable at
/// start). Network trouble never ends the process.
use chronicle_lib::nordic::live::{
    handle_message_with, Companies, Counters, IngestedCheck, Listener, Registered, SeenSet,
    SignalDb, SlackMode, DEFAULT_WS_URL, SEEN_CAPACITY,
};
use chronicle_lib::nordic::live_slack::{
    describe_metrics, enqueue, jobs_for, run_worker, Sessions, SlackApi, SlackJob, QUEUE_CAPACITY,
};
use chronicle_lib::nordic::slack::{SlackClient, DEFAULT_API_URL};
use clap::Parser;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Row};
use std::process::ExitCode;
use std::str::FromStr;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
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

    /// Slack bot token (`chat:write`). Without it, or without `SLACK_CHANNEL`,
    /// Slack posting is off.
    #[arg(long, env = "SLACK_BOT_TOKEN", hide_env_values = true)]
    slack_bot_token: Option<String>,

    /// Slack channel ID (for example `C0123456789`; IDs survive renames).
    #[arg(long, env = "SLACK_CHANNEL")]
    slack_channel: Option<String>,

    /// Slack Web API base URL.
    #[arg(long, env = "SLACK_API_URL", default_value = DEFAULT_API_URL)]
    slack_api_url: String,

    /// Port of the Prometheus `/metrics` endpoint.
    #[arg(long, env = "METRICS_PORT", default_value_t = 8080)]
    metrics_port: u16,

    /// `json` (default, one JSON object per line) or `text`.
    #[arg(long, env = "LOG_FORMAT", default_value = "json")]
    log_format: String,
}

async fn load_companies(pool: &PgPool) -> sqlx::Result<Companies> {
    let rows = sqlx::query(
        "SELECT ticker, exchange_mic, mfn_slug, name FROM companies WHERE mfn_slug IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;
    Ok(Companies::new(
        rows.iter()
            .map(|r| Registered {
                ticker: r.get("ticker"),
                exchange_mic: r.get("exchange_mic"),
                mfn_slug: r.get("mfn_slug"),
                name: r.get("name"),
            })
            .collect(),
    ))
}

/// Loads the exchange calendars from signal's database. Keeps `previous` when
/// the load fails, and starts empty (no session labels) without a database.
async fn load_sessions(signal: Option<&SignalDb>, previous: &Sessions) -> Sessions {
    let Some(db) = signal else {
        return Sessions::default();
    };
    match Sessions::load(&db.0).await {
        Ok(s) => {
            if s.len() != previous.len() {
                info!(exchanges = s.len(), "exchange calendars loaded");
            }
            s
        }
        Err(e) => {
            warn!(error = %e, "loading exchange calendars failed, session labels use the last load");
            previous.clone()
        }
    }
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

/// Serves `GET /metrics` (and a plain 200 elsewhere, for probes).
async fn serve_metrics(port: u16, handle: PrometheusHandle) {
    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            warn!(error = %e, port, "metrics endpoint not started");
            return;
        }
    };
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let handle = handle.clone();
        tokio::spawn(async move {
            let (read, mut write) = stream.into_split();
            let mut line = String::new();
            if BufReader::new(read).read_line(&mut line).await.is_err() {
                return;
            }
            let body = if line.starts_with("GET /metrics") {
                handle.render()
            } else {
                "ok\n".to_string()
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain; version=0.0.4\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = write.write_all(response.as_bytes()).await;
        });
    }
}

async fn run(args: Args) -> anyhow::Result<()> {
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .connect(&args.database_url)
        .await?;
    // The Slack claim table lives in chronicle's migrations; like the other
    // binaries, apply them so a fresh database works without waiting for the poll.
    sqlx::migrate!("./migrations").run(&pool).await?;
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

    let metrics = PrometheusBuilder::new().install_recorder()?;
    describe_metrics();
    tokio::spawn(serve_metrics(args.metrics_port, metrics));

    let slack_tx = match (&args.slack_bot_token, &args.slack_channel) {
        (Some(token), Some(channel)) if !token.is_empty() && !channel.is_empty() => {
            let client = SlackClient::with_api_url(token, channel, &args.slack_api_url)?;
            let (jobs_tx, jobs_rx) = mpsc::channel::<SlackJob>(QUEUE_CAPACITY);
            tokio::spawn(run_worker(
                pool.clone(),
                Box::new(client) as Box<dyn SlackApi>,
                jobs_rx,
            ));
            info!(channel = %channel, "Slack posting enabled");
            Some(jobs_tx)
        }
        _ => {
            warn!("Slack disabled: SLACK_BOT_TOKEN or SLACK_CHANNEL is not set");
            None
        }
    };
    let mode = if slack_tx.is_some() {
        SlackMode::Enabled
    } else {
        SlackMode::Disabled
    };
    let mut sessions = load_sessions(signal.as_ref(), &Sessions::default()).await;

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
                let (counters, dispositions) = handle_message_with(
                    &msg.text, msg.received_at, &companies, &mut seen, check, mode,
                )
                .await;
                window.seen += counters.seen;
                window.relevant += counters.relevant;
                if let Some(jobs) = &slack_tx {
                    for job in jobs_for(dispositions, &sessions) {
                        enqueue(jobs, job);
                    }
                }
            }
            _ = reload.tick() => {
                match load_companies(&pool).await {
                    Ok(c) => {
                        if c.len() != companies.len() {
                            info!(old = companies.len(), new = c.len(), "registered companies changed");
                        }
                        companies = c;
                    }
                    Err(e) => warn!(error = %e, "reloading companies failed, keeping the old list"),
                }
                sessions = load_sessions(signal.as_ref(), &sessions).await;
            }
            _ = stats.tick() => {
                window.log();
                window = Counters::default();
            }
            _ = tokio::signal::ctrl_c() => { info!("interrupted, shutting down"); return Ok(()); }
            _ = term.recv() => { info!("terminated, shutting down"); return Ok(()); }
        }
    }
}
