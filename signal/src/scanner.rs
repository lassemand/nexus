/// Daily prospective scanner: two-step insider fingerprint detection.
///
/// Step 1 — gate on recent insider purchases (insider_filings).
/// Step 2 — compute rolling pre-event features and score with ONNX model.
/// Results written to ticker_scores.
mod features;

use chrono::{Days, Utc};
use clap::Parser;
use ndarray::Array2;
use ort::{session::Session, value::TensorRef};
use sqlx::{postgres::PgPoolOptions, Row};
use tracing::{info, warn};

use features::{compute_pre_event_features, Bar};

/// Feature order — must match ml/train_insider_score.py FEATURE_NAMES.
const FEATURE_NAMES: &[&str] = &[
    "car_20d",
    "car_10d",
    "car_5d",
    "mean_abvol_20d",
    "max_abvol_20d",
    "realized_vol_20d",
    "price_momentum_20d",
    "vol_trend_slope",
];

#[derive(Parser)]
#[command(about = "Daily insider fingerprint scanner")]
struct Args {
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    #[arg(long, env = "INSIDER_MODEL_PATH")]
    model_path: String,

    #[arg(long, env = "LOOKBACK_DAYS", default_value = "30")]
    lookback_days: u64,

    #[arg(long, env = "THRESHOLD", default_value = "0.0")]
    threshold: f32,
}

/// Fetch tickers with recent insider purchases and their triggering person_ids.
/// Returns Vec<(ticker, person_ids)> — purchases only (transaction_code = 'P').
async fn fetch_recent_insider_activity(
    pool: &sqlx::PgPool,
    since: chrono::NaiveDate,
) -> sqlx::Result<Vec<(String, Vec<i64>)>> {
    let rows = sqlx::query(
        r#"
        SELECT ticker,
               array_agg(DISTINCT person_id ORDER BY person_id) AS person_ids
        FROM insider_filings
        WHERE transaction_date >= $1
          AND transaction_code = 'P'
        GROUP BY ticker
        "#,
    )
    .bind(since)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| (r.get("ticker"), r.get("person_ids")))
        .collect())
}

/// Fetch up to `limit` bars before `before_date` for `ticker`, chronological order.
async fn fetch_bars_before(
    pool: &sqlx::PgPool,
    ticker: &str,
    before_date: chrono::NaiveDate,
    limit: i64,
) -> sqlx::Result<Vec<Bar>> {
    let rows = sqlx::query(
        "SELECT close, volume FROM bars \
         WHERE ticker = $1 AND date < $2 \
         ORDER BY date DESC LIMIT $3",
    )
    .bind(ticker)
    .bind(before_date)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let mut bars: Vec<Bar> = rows
        .into_iter()
        .map(|r| Bar {
            close: r.get("close"),
            volume: r.get("volume"),
        })
        .collect();
    bars.reverse();
    Ok(bars)
}

/// Insert a ticker_scores row; ignore if (ticker, score_date) already exists.
#[allow(clippy::too_many_arguments)]
async fn insert_ticker_score(
    pool: &sqlx::PgPool,
    ticker: &str,
    score_date: chrono::NaiveDate,
    insider_score: f64,
    person_ids: &[i64],
    values: &[f32; 8],
) -> sqlx::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO ticker_scores (
            ticker, score_date, insider_score, triggering_person_ids,
            car_20d, car_10d, car_5d,
            mean_abvol_20d, max_abvol_20d, realized_vol_20d,
            price_momentum_20d, vol_trend_slope
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
        ON CONFLICT (ticker, score_date) DO NOTHING
        "#,
    )
    .bind(ticker)
    .bind(score_date)
    .bind(insider_score)
    .bind(person_ids)
    .bind(values[0] as f64)
    .bind(values[1] as f64)
    .bind(values[2] as f64)
    .bind(values[3] as f64)
    .bind(values[4] as f64)
    .bind(values[5] as f64)
    .bind(values[6] as f64)
    .bind(values[7] as f64)
    .execute(pool)
    .await?;
    Ok(())
}

/// Run ONNX inference and return the positive-class probability.
fn score_features(session: &mut Session, values: &[f32; 8]) -> Option<f32> {
    let input = Array2::from_shape_vec((1, FEATURE_NAMES.len()), values.to_vec()).ok()?;
    let tensor = TensorRef::from_array_view(input.view()).ok()?;
    let outputs = session.run(ort::inputs![tensor]).ok()?;
    let (_shape, data) = outputs[1].try_extract_tensor::<f32>().ok()?;
    data.get(1).copied()
}

#[tokio::main]
async fn main() {
    dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/.env")).ok();
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    // Load ONNX model — hard exit if missing (model must exist for scanner to run).
    if !std::path::Path::new(&args.model_path).exists() {
        tracing::error!(path = %args.model_path, "ONNX model not found — aborting");
        std::process::exit(1);
    }
    let mut session = Session::builder()
        .and_then(|mut b| b.commit_from_file(&args.model_path))
        .expect("failed to load ONNX model");
    info!(path = %args.model_path, "insider score model loaded");

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&args.database_url)
        .await
        .expect("failed to connect to postgres");

    let today = Utc::now().date_naive();
    let since = today - Days::new(args.lookback_days);

    // ── Step 1: gate on recent insider purchases ──────────────────────────────

    let candidates = fetch_recent_insider_activity(&pool, since)
        .await
        .expect("failed to fetch insider activity");

    info!(
        "Step 1 gate: {} tickers with recent insider purchases",
        candidates.len()
    );

    if candidates.is_empty() {
        warn!("no tickers passed Step 1 gate — exiting");
        return;
    }

    // ── Step 2: score each candidate ─────────────────────────────────────────

    let mut scored = 0usize;
    for (ticker, person_ids) in &candidates {
        let bars = match fetch_bars_before(&pool, ticker, today, 80).await {
            Ok(b) => b,
            Err(e) => {
                warn!(ticker = %ticker, error = %e, "failed to fetch bars, skipping");
                continue;
            }
        };

        let f = compute_pre_event_features(&bars);

        // Extract feature vector — skip if any feature is None.
        let feature_vec: Option<[f32; 8]> = (|| {
            Some([
                f.car_20d? as f32,
                f.car_10d? as f32,
                f.car_5d? as f32,
                f.mean_abvol_20d? as f32,
                f.max_abvol_20d? as f32,
                f.realized_vol_20d? as f32,
                f.price_momentum_20d? as f32,
                f.vol_trend_slope? as f32,
            ])
        })();

        let values = match feature_vec {
            Some(v) => v,
            None => {
                warn!(ticker = %ticker, bars = bars.len(), "insufficient bar history for feature computation, skipping");
                continue;
            }
        };

        let score = match score_features(&mut session, &values) {
            Some(s) => s,
            None => {
                warn!(ticker = %ticker, "ONNX inference failed, skipping");
                continue;
            }
        };

        if score < args.threshold {
            continue;
        }

        match insert_ticker_score(&pool, ticker, today, score as f64, person_ids, &values).await {
            Ok(()) => {
                info!(
                    ticker = %ticker,
                    score,
                    person_ids = ?person_ids,
                    "ticker score written"
                );
                scored += 1;
            }
            Err(e) => warn!(ticker = %ticker, error = %e, "failed to insert ticker score"),
        }
    }

    info!(
        "scanner complete: {scored}/{} tickers scored above threshold",
        candidates.len()
    );
}
