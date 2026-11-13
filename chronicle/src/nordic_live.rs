use chronicle_lib::nordic::calendar::MfnClient;
/// Live MFN listener (NEX-170): logs Nordic releases the moment they are published
/// and, with `KAFKA_BROKERS`, emits each company announcement to `special.events`
/// within seconds (NEX-241; queue and producer in `chronicle_lib::nordic::live_emit`).
///
/// Connects to MFN's unofficial WebSocket (`wss://mfn.se/all/s/nordic.json`),
/// picks out items from companies registered with an `mfn_slug`, classifies them
/// with the same rules as the hourly `nordic-reports` poll and logs one
/// structured event per new item (`event = "report_released"` for results
/// releases, `"company_news"` otherwise). With `SLACK_BOT_TOKEN`, results releases are also posted
/// to Slack channel `SLACK_CHANNEL` (NEX-222) and company announcements to
/// `SLACK_ANNOUNCEMENTS_CHANNEL` (NEX-239), each independently; the only
/// thing it writes is the `slack_report_posts` claim. The poll stays the
/// fallback for anything missed while disconnected. See `chronicle_lib::nordic::live`
/// and `chronicle_lib::nordic::live_slack`.
///
/// Exits non-zero only on a startup error (bad config, database unreachable at
/// start). Network trouble never ends the process.
use chronicle_lib::nordic::events::{KafkaSink, SPECIAL_EVENTS_TOPIC};
use chronicle_lib::nordic::live::{
    describe_metrics, describe_publish_metrics, handle_catchup_items, handle_message_full,
    publications, publish_live, Companies, Counters, Handling, IngestedCheck, Listener, Registered,
    SeenSet, SignalDb, SlackChannels, DEFAULT_SLACK_MAX_AGE, DEFAULT_WS_URL, LATENCY_BUCKETS,
    SEEN_CAPACITY,
};
use chronicle_lib::nordic::live_catchup::{
    self, describe_catchup_metrics, page_backoff, run_coordinator, run_once, Fetched,
    SharedWatermark, Watermark, DEFAULT_FEED_URL, FLUSH_EVERY, PAGE_GAP, PAGE_TIMEOUT, USER_AGENT,
};
use chronicle_lib::nordic::live_emit::{
    describe_emit_metrics, producer_backoff, run_producer, Emitter, EMIT_LATENCY_BUCKETS,
    QUEUE_CAPACITY as EMIT_QUEUE_CAPACITY,
};
use chronicle_lib::nordic::live_slack::{
    describe_metrics as describe_slack_metrics, enqueue, jobs_for_fresh, run_worker, Sessions,
    SlackApi, SlackJob, QUEUE_CAPACITY,
};
use chronicle_lib::nordic::slack::{SlackClient, DEFAULT_API_URL};
use chrono::Utc;
use clap::Parser;
use metrics::{counter, gauge};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Row};
use std::process::ExitCode;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use telemetry::trace::LogFormat;
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

    /// Kafka brokers. When set, released reports are published to
    /// `special.events` the moment they are seen, so they leave "upcoming"
    /// without waiting for the nightly poll. Unset: log only.
    #[arg(long, env = "KAFKA_BROKERS")]
    kafka_brokers: Option<String>,

    #[arg(long, env = "SPECIAL_EVENTS_TOPIC", default_value = SPECIAL_EVENTS_TOPIC)]
    special_events_topic: String,

    /// Slack bot token (`chat:write`). Without it, or without any channel,
    /// Slack posting is off.
    #[arg(long, env = "SLACK_BOT_TOKEN", hide_env_values = true)]
    slack_bot_token: Option<String>,

    /// Slack channel ID (for example `C0123456789`; IDs survive renames) that
    /// results releases are posted to. Unset: reports are not posted.
    #[arg(long, env = "SLACK_CHANNEL")]
    slack_channel: Option<String>,

    /// Slack channel ID that company announcements are posted to. Unset:
    /// announcements are not posted. Independent of `SLACK_CHANNEL`.
    #[arg(long, env = "SLACK_ANNOUNCEMENTS_CHANNEL")]
    slack_announcements_channel: Option<String>,

    /// Slack Web API base URL.
    #[arg(long, env = "SLACK_API_URL", default_value = DEFAULT_API_URL)]
    slack_api_url: String,

    /// MFN's HTTP feed of the Nordic stream, read after every (re)connect to
    /// catch up on items the socket missed.
    #[arg(long, env = "MFN_FEED_URL", default_value = DEFAULT_FEED_URL)]
    feed_url: String,

    /// Releases and announcements published longer ago than this are never
    /// posted to Slack (they are still emitted and logged).
    #[arg(long, env = "SLACK_MAX_AGE_MINUTES", default_value_t = DEFAULT_SLACK_MAX_AGE.as_secs() / 60)]
    slack_max_age_minutes: u64,

    /// Port of the Prometheus `/metrics` endpoint.
    #[arg(long, env = "METRICS_PORT", default_value_t = 9464)]
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
    let format = if args.log_format == "text" {
        LogFormat::Text
    } else {
        LogFormat::Json
    };
    let _tracing = match telemetry::trace::init_with("nordic-live", format) {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("nordic-live: initialising logging failed: {e:#}");
            return ExitCode::FAILURE;
        }
    };

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

    telemetry::serve_with_buckets(
        "nordic-live",
        args.metrics_port,
        &[
            ("nordic_live_release_latency_seconds", LATENCY_BUCKETS),
            ("nordic_live_emit_latency_seconds", EMIT_LATENCY_BUCKETS),
        ],
    )
    .await?;
    describe_metrics();
    describe_slack_metrics();
    describe_publish_metrics();
    describe_emit_metrics();
    describe_catchup_metrics();
    gauge!("nordic_live_registered_companies").set(companies.len() as f64);

    let events: Option<Arc<KafkaSink>> = match args.kafka_brokers.as_deref() {
        Some(brokers) if !brokers.is_empty() => {
            let sink = KafkaSink::low_latency(brokers, args.special_events_topic.clone())?;
            info!(topic = %args.special_events_topic,
                  "publishing released reports and company announcements to special.events");
            Some(Arc::new(sink))
        }
        _ => {
            warn!("KAFKA_BROKERS not set: emission is off, reports and announcements wait for the poll");
            None
        }
    };

    let emitter: Option<Emitter> = events.as_ref().map(|sink| {
        let (emitter, jobs) = Emitter::channel(EMIT_QUEUE_CAPACITY);
        tokio::spawn(run_producer(sink.clone(), jobs, producer_backoff()));
        emitter
    });

    let token = args.slack_bot_token.as_deref().filter(|t| !t.is_empty());
    let channels = match token {
        Some(_) => SlackChannels::new(
            args.slack_channel.clone(),
            args.slack_announcements_channel.clone(),
        ),
        None => SlackChannels::default(),
    };
    match (&channels.reports, &channels.announcements) {
        (Some(r), _) => info!(channel = %r, "Slack report posting enabled"),
        (None, _) => {
            warn!("Slack report posting disabled: SLACK_BOT_TOKEN or SLACK_CHANNEL is not set")
        }
    }
    match &channels.announcements {
        Some(c) => info!(channel = %c, "Slack announcement posting enabled"),
        None => {
            info!("Slack announcement posting disabled: SLACK_ANNOUNCEMENTS_CHANNEL is not set")
        }
    }
    let slack_tx = match token {
        Some(token) => {
            let client = SlackClient::with_api_url(token, &args.slack_api_url)?;
            let (jobs_tx, jobs_rx) = mpsc::channel::<SlackJob>(QUEUE_CAPACITY);
            tokio::spawn(run_worker(
                pool.clone(),
                Box::new(client) as Box<dyn SlackApi>,
                jobs_rx,
            ));
            Some(jobs_tx)
        }
        _ => None,
    };
    let mut sessions = load_sessions(signal.as_ref(), &Sessions::default()).await;

    let watermark: SharedWatermark = Arc::new(std::sync::Mutex::new(
        match Watermark::load(&pool, Utc::now()).await {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "loading the watermark failed, starting 15 minutes back");
                Watermark::new(Utc::now() - live_catchup::DEFAULT_WATERMARK_AGE)
            }
        },
    ));

    let (tx, mut rx) = mpsc::channel(256);
    let (conn_tx, conn_rx) = mpsc::unbounded_channel();
    let (catchup_tx, mut catchup_rx) = mpsc::channel::<Fetched>(4);
    let feed_url = reqwest::Url::parse(&args.feed_url)?;
    let feed_origin = feed_url.origin().ascii_serialization();
    let catchup_client = Arc::new(tokio::sync::Mutex::new(MfnClient::with_options(
        feed_origin,
        PAGE_GAP,
        USER_AGENT,
        PAGE_TIMEOUT,
    )?));
    tokio::spawn({
        let watermark = watermark.clone();
        async move {
            run_coordinator(conn_rx, move || {
                let (client, url, wm, out) = (
                    catchup_client.clone(),
                    feed_url.clone(),
                    watermark.clone(),
                    catchup_tx.clone(),
                );
                async move { run_once(&client, &url, &wm, &out, page_backoff()).await }
            })
            .await
        }
    });
    let mut listener = Listener::new(args.ws_url.clone(), USER_AGENT);
    listener.events = Some(conn_tx);
    tokio::spawn(async move { listener.run(tx).await });
    let slack_max_age = Duration::from_secs(args.slack_max_age_minutes * 60);

    let mut seen = SeenSet::new(SEEN_CAPACITY);
    let mut window = Counters::default();
    let mut reload = tokio::time::interval(RELOAD_EVERY);
    let mut stats = tokio::time::interval(STATS_EVERY);
    let mut flush = tokio::time::interval(FLUSH_EVERY);
    reload.tick().await;
    stats.tick().await;
    flush.tick().await;
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    loop {
        tokio::select! {
            received = rx.recv() => {
                let Some(msg) = received else { return Ok(()) };
                let check = signal.as_ref().map(|s| s as &dyn IngestedCheck);
                let handling = Handling {
                    check,
                    channels: &channels,
                    emitter: emitter.as_ref(),
                    slack_max_age,
                };
                let handled = handle_message_full(
                    &msg.text, msg.received_at, &companies, &mut seen, &handling,
                )
                .await;
                watermark.lock().expect("watermark lock is never poisoned")
                    .observe(handled.newest_published);
                let (counters, dispositions) = (handled.counters, handled.dispositions);
                window.seen += counters.seen;
                window.relevant += counters.relevant;
                // Independent of Slack, and off the socket loop: a slow broker
                // never delays the next message.
                if let Some(sink) = &events {
                    for p in publications(&dispositions) {
                        let sink = sink.clone();
                        tokio::spawn(async move {
                            publish_live(sink.as_ref(), &p.key, &p.event, p.span.span()).await
                        });
                    }
                }
                if let Some(jobs) = &slack_tx {
                    for job in jobs_for_fresh(dispositions, &sessions, &channels, slack_max_age) {
                        enqueue(jobs, job);
                    }
                }
            }
            Some(fetched) = catchup_rx.recv() => {
                let check = signal.as_ref().map(|s| s as &dyn IngestedCheck);
                let handling = Handling {
                    check,
                    channels: &channels,
                    emitter: emitter.as_ref(),
                    slack_max_age,
                };
                let handled = handle_catchup_items(
                    fetched.items.clone(), Utc::now(), &companies, &mut seen, &handling,
                )
                .await;
                let after = {
                    let mut w = watermark.lock().expect("watermark lock is never poisoned");
                    w.observe(handled.newest_published);
                    w.value()
                };
                live_catchup::log_run(&fetched, handled.dispositions.len(), after);
                if let Some(sink) = &events {
                    for p in publications(&handled.dispositions) {
                        let sink = sink.clone();
                        tokio::spawn(async move {
                            publish_live(sink.as_ref(), &p.key, &p.event, p.span.span()).await
                        });
                    }
                }
                if let Some(jobs) = &slack_tx {
                    for job in jobs_for_fresh(handled.dispositions, &sessions, &channels, slack_max_age) {
                        enqueue(jobs, job);
                    }
                }
            }
            _ = flush.tick() => {
                live_catchup::flush(&pool, &watermark, Utc::now()).await;
            }
            _ = reload.tick() => {
                match load_companies(&pool).await {
                    Ok(c) => {
                        if c.len() != companies.len() {
                            info!(old = companies.len(), new = c.len(), "registered companies changed");
                        }
                        companies = c;
                        gauge!("nordic_live_registered_companies").set(companies.len() as f64);
                    }
                    Err(e) => {
                        counter!("nordic_live_company_reload_failures_total").increment(1);
                        warn!(error = %e, "reloading companies failed, keeping the old list");
                    }
                }
                sessions = load_sessions(signal.as_ref(), &sessions).await;
            }
            _ = stats.tick() => {
                window.log();
                window = Counters::default();
            }
            _ = tokio::signal::ctrl_c() => {
                info!("interrupted, shutting down");
                live_catchup::flush(&pool, &watermark, Utc::now()).await;
                return Ok(());
            }
            _ = term.recv() => {
                info!("terminated, shutting down");
                live_catchup::flush(&pool, &watermark, Utc::now()).await;
                return Ok(());
            }
        }
    }
}
