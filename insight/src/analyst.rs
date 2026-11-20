//! Queries over the analyst's simulated trades (`simulated_trades`,
//! `simulated_trade_observations`, `analyst_decisions`; NEX-257).
//!
//! All arithmetic is done by Postgres in `NUMERIC` and the figures are returned
//! as decimal strings, so no digit passes through a float. Only `closed`
//! trades enter the P&L figures; the others are counted and reported as
//! excluded.

use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::PgPool;

/// What the results are, repeated in the description of each tool.
pub const CAVEATS: &str = "Forward-only and simulated: each result exists only because the system decided live, before the market reacted, so there is no look-ahead bias, and results cannot be backfilled. \
Per-share returns, no fees, no position sizing. For releases published before the open the figures measure the move after the open. \
Shadow trades (no_buy, abstain, skipped_no_kernel) show what would have happened without acting; they are not actions taken.";

/// The default and the largest number of trades `fetch_analyst_trades` returns.
pub const DEFAULT_LIMIT: i64 = 200;
const MAX_LIMIT: i64 = 1000;

/// Seconds either side of a trade's publish instant within which a stored release of the same
/// company counts as the same release (signal's `TWIN_WINDOW_SECS`: the analyst decides on the
/// first language copy, and signal later replaces the stored `source_id` with the English copy's).
const TWIN_WINDOW_SECS: i32 = 120;

/// Parses a `since` argument: an RFC 3339 instant or a `YYYY-MM-DD` date (UTC midnight).
pub fn parse_since(value: &str) -> Result<DateTime<Utc>, String> {
    let value = value.trim();
    if let Ok(at) = DateTime::parse_from_rfc3339(value) {
        return Ok(at.with_timezone(&Utc));
    }
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|d| d.and_utc())
        .ok_or_else(|| format!("invalid since {value:?}: use RFC 3339 or YYYY-MM-DD"))
}

/// One trade with its decision.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct TradeRow {
    trade_id: i64,
    news_id: String,
    ticker: String,
    exchange_mic: String,
    decision: String,
    shadow: bool,
    reason: Option<String>,
    fiscal_period: Option<String>,
    published_at: DateTime<Utc>,
    status: String,
    version: i64,
    entry_at: Option<DateTime<Utc>>,
    entry_ask: Option<String>,
    exit_at: Option<DateTime<Utc>>,
    exit_bid: Option<String>,
    pnl_per_share: Option<String>,
    pnl_pct: Option<String>,
    currency: Option<String>,
    release_to_entry_ms: Option<i64>,
    trace_id: Option<String>,
    trade_trace_id: Option<String>,
}

/// The trades matching the filters, newest first.
pub async fn fetch_trades(
    pool: &PgPool,
    ticker: Option<&str>,
    decision: Option<&str>,
    since: Option<DateTime<Utc>>,
    include_shadow: bool,
    limit: Option<i64>,
) -> sqlx::Result<Vec<TradeRow>> {
    sqlx::query_as(
        r#"
        SELECT t.trade_id, t.news_id, t.ticker, t.exchange_mic, t.decision, t.shadow,
               d.reason, d.fiscal_period, t.published_at, t.status, t.version,
               t.entry_at, t.entry_ask::text AS entry_ask, t.exit_at, t.exit_bid::text AS exit_bid,
               t.pnl_per_share::text AS pnl_per_share, t.pnl_pct::text AS pnl_pct, t.currency,
               t.release_to_entry_ms, COALESCE(t.trace_id, d.trace_id) AS trace_id, t.trade_trace_id
        FROM simulated_trades t
        LEFT JOIN analyst_decisions d ON d.news_id = t.news_id
        WHERE ($1::text IS NULL OR t.ticker = $1)
          AND ($2::text IS NULL OR t.decision = $2)
          AND ($3::timestamptz IS NULL OR t.published_at >= $3)
          AND ($4 OR NOT t.shadow)
        ORDER BY t.published_at DESC, t.trade_id DESC
        LIMIT $5
        "#,
    )
    .bind(ticker)
    .bind(decision)
    .bind(since)
    .bind(include_shadow)
    .bind(limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT))
    .fetch_all(pool)
    .await
}

#[derive(sqlx::FromRow)]
struct StatusCount {
    decision: String,
    status: String,
    n: i64,
}

#[derive(sqlx::FromRow)]
struct HorizonStats {
    decision: String,
    horizon: String,
    n: i64,
    mean_pnl_pct: Option<String>,
    median_pnl_pct: Option<String>,
    share_positive_pct: Option<String>,
}

#[derive(sqlx::FromRow)]
struct SessionStats {
    release_session: String,
    n: i64,
    median_release_to_entry_ms: Option<String>,
}

#[derive(sqlx::FromRow)]
struct LateStats {
    observations: i64,
    late: i64,
    missed: i64,
    late_or_missed_pct: Option<String>,
}

fn horizon_stats(rows: &[HorizonStats]) -> Vec<Value> {
    rows.iter()
        .map(|r| {
            json!({
                "decision": r.decision,
                "horizon": r.horizon,
                "n": r.n,
                "mean_pnl_pct": r.mean_pnl_pct,
                "median_pnl_pct": r.median_pnl_pct,
                "share_positive_pct": r.share_positive_pct,
            })
        })
        .collect()
}

/// The summary of the trades matching the filters.
pub async fn summarise(
    pool: &PgPool,
    since: Option<DateTime<Utc>>,
    ticker: Option<&str>,
) -> sqlx::Result<Value> {
    let counts: Vec<StatusCount> = sqlx::query_as(
        r#"
        SELECT t.decision, t.status, COUNT(*)::bigint AS n
        FROM simulated_trades t
        WHERE ($1::timestamptz IS NULL OR t.published_at >= $1)
          AND ($2::text IS NULL OR t.ticker = $2)
        GROUP BY t.decision, t.status
        ORDER BY t.decision, t.status
        "#,
    )
    .bind(since)
    .bind(ticker)
    .fetch_all(pool)
    .await?;

    let horizons: Vec<HorizonStats> = sqlx::query_as(
        r#"
        WITH returns AS (
            SELECT t.decision, o.horizon,
                   (o.bid - t.entry_ask) / t.entry_ask * 100 AS r
            FROM simulated_trades t
            JOIN simulated_trade_observations o ON o.trade_id = t.trade_id
            WHERE t.status = 'closed' AND t.entry_ask > 0
              AND o.status = 'ok' AND NOT o.late AND o.bid IS NOT NULL
              AND ($1::timestamptz IS NULL OR t.published_at >= $1)
              AND ($2::text IS NULL OR t.ticker = $2)
        ), ranked AS (
            SELECT decision, horizon, r,
                   ROW_NUMBER() OVER w AS rn, COUNT(*) OVER (PARTITION BY decision, horizon) AS n
            FROM returns
            WINDOW w AS (PARTITION BY decision, horizon ORDER BY r)
        )
        SELECT decision, horizon, MAX(n)::bigint AS n,
               ROUND(AVG(r), 4)::text AS mean_pnl_pct,
               ROUND(AVG(r) FILTER (WHERE rn IN ((n + 1) / 2, (n + 2) / 2)), 4)::text AS median_pnl_pct,
               ROUND(COUNT(*) FILTER (WHERE r > 0) * 100.0 / COUNT(*), 2)::text AS share_positive_pct
        FROM ranked
        GROUP BY decision, horizon
        ORDER BY decision, MIN(CASE horizon WHEN '1m' THEN 1 WHEN '5m' THEN 2 WHEN '15m' THEN 3
            WHEN '30m' THEN 4 WHEN '60m' THEN 5 WHEN 'hold' THEN 6 WHEN 'close' THEN 7 ELSE 8 END), horizon
        "#,
    )
    .bind(since)
    .bind(ticker)
    .fetch_all(pool)
    .await?;

    let sessions: Vec<SessionStats> = sqlx::query_as(
        r#"
        WITH entered AS (
            SELECT t.release_to_entry_ms,
                   COALESCE((SELECT se.release_session FROM special_events se
                             WHERE se.ticker = t.ticker AND se.exchange_mic = t.exchange_mic
                               AND se.status = 'occurred' AND se.release_session IS NOT NULL
                               AND se.occurred_at BETWEEN t.published_at - make_interval(secs => $3)
                                                      AND t.published_at + make_interval(secs => $3)
                             ORDER BY (se.source_id = t.news_id) DESC,
                                      abs(extract(epoch FROM se.occurred_at - t.published_at)), se.id
                             LIMIT 1), 'unknown') AS release_session
            FROM simulated_trades t
            WHERE t.release_to_entry_ms IS NOT NULL
              AND ($1::timestamptz IS NULL OR t.published_at >= $1)
              AND ($2::text IS NULL OR t.ticker = $2)
        ), ranked AS (
            SELECT release_session, release_to_entry_ms AS v,
                   ROW_NUMBER() OVER (PARTITION BY release_session ORDER BY release_to_entry_ms) AS rn,
                   COUNT(*) OVER (PARTITION BY release_session) AS n
            FROM entered
        )
        SELECT release_session, MAX(n)::bigint AS n,
               ROUND(AVG(v::numeric) FILTER (WHERE rn IN ((n + 1) / 2, (n + 2) / 2)), 1)::text
                   AS median_release_to_entry_ms
        FROM ranked
        GROUP BY release_session
        ORDER BY release_session
        "#,
    )
    .bind(since)
    .bind(ticker)
        .bind(f64::from(TWIN_WINDOW_SECS))
    .fetch_all(pool)
    .await?;

    let late: LateStats = sqlx::query_as(
        r#"
        SELECT COUNT(*)::bigint AS observations,
               COUNT(*) FILTER (WHERE o.late)::bigint AS late,
               COUNT(*) FILTER (WHERE o.status = 'missed')::bigint AS missed,
               ROUND(COUNT(*) FILTER (WHERE o.late OR o.status = 'missed') * 100.0
                     / NULLIF(COUNT(*), 0), 2)::text AS late_or_missed_pct
        FROM simulated_trades t
        JOIN simulated_trade_observations o ON o.trade_id = t.trade_id
        WHERE ($1::timestamptz IS NULL OR t.published_at >= $1)
          AND ($2::text IS NULL OR t.ticker = $2)
        "#,
    )
    .bind(since)
    .bind(ticker)
    .fetch_one(pool)
    .await?;

    let total: i64 = counts.iter().map(|c| c.n).sum();
    let closed: i64 = counts
        .iter()
        .filter(|c| c.status == "closed")
        .map(|c| c.n)
        .sum();
    let mut excluded: Vec<(String, i64)> = Vec::new();
    for c in counts.iter().filter(|c| c.status != "closed") {
        match excluded.iter_mut().find(|(s, _)| *s == c.status) {
            Some((_, n)) => *n += c.n,
            None => excluded.push((c.status.clone(), c.n)),
        }
    }
    let side_by_side: Vec<Value> = {
        let mut horizon_names: Vec<&str> = Vec::new();
        for h in &horizons {
            if !horizon_names.contains(&h.horizon.as_str()) {
                horizon_names.push(&h.horizon);
            }
        }
        horizon_names
            .into_iter()
            .map(|name| {
                let pick = |decision: &str| {
                    horizons
                        .iter()
                        .find(|h| h.horizon == name && h.decision == decision)
                        .map(|h| {
                            json!({
                                "n": h.n,
                                "mean_pnl_pct": h.mean_pnl_pct,
                                "median_pnl_pct": h.median_pnl_pct,
                                "share_positive_pct": h.share_positive_pct,
                            })
                        })
                };
                json!({ "horizon": name, "buy": pick("buy"), "no_buy": pick("no_buy") })
            })
            .collect()
    };

    Ok(json!({
        "caveats": CAVEATS,
        "counts": counts.iter()
            .map(|c| json!({ "decision": c.decision, "status": c.status, "n": c.n }))
            .collect::<Vec<_>>(),
        "pnl": {
            "basis": "closed trades only; pnl_pct = (bid at horizon - entry_ask) / entry_ask * 100, from observations with status ok",
            "closed_trades": closed,
            "excluded_trades": {
                "total": total - closed,
                "reason": "status is not closed (no entry or exit quote, exit missed, calendar unknown, currency mismatch, quote too old, still pending or open)",
                "by_status": excluded.iter()
                    .map(|(status, n)| json!({ "status": status, "n": n }))
                    .collect::<Vec<_>>(),
            },
            "by_decision_and_horizon": horizon_stats(&horizons),
            "buy_vs_no_buy": side_by_side,
        },
        "release_to_entry": {
            "unit": "milliseconds; median per release session (pre_open, intraday, after_close, non_trading_day, unknown when the reaction is not computed)",
            "by_release_session": sessions.iter()
                .map(|s| json!({
                    "release_session": s.release_session,
                    "n": s.n,
                    "median_ms": s.median_release_to_entry_ms,
                }))
                .collect::<Vec<_>>(),
        },
        "observations": {
            "total": late.observations,
            "late": late.late,
            "missed": late.missed,
            "late_or_missed_pct": late.late_or_missed_pct,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn seed(
        pool: &PgPool,
        trade_id: i64,
        decision: &str,
        status: &str,
        entry: &str,
        bid_30m: &str,
    ) {
        sqlx::query(
            r#"INSERT INTO simulated_trades
               (trade_id, news_id, ticker, exchange_mic, decision, shadow, published_at,
                hold_minutes, entry_at, entry_ask, status, version, release_to_entry_ms)
               VALUES ($1, $2, 'AAA', 'XSTO', $3, $3 <> 'buy', now(), 120, now(), $4::numeric, $5, 3, $6)"#,
        )
        .bind(trade_id)
        .bind(format!("n{trade_id}"))
        .bind(decision)
        .bind(entry)
        .bind(status)
        .bind(trade_id * 100)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            r#"INSERT INTO simulated_trade_observations
               (trade_id, horizon, scheduled_at, bid, late, status)
               VALUES ($1, '30m', now(), $2::numeric, $3, 'ok')"#,
        )
        .bind(trade_id)
        .bind(bid_30m)
        .bind(status == "no_exit_quote")
        .execute(pool)
        .await
        .unwrap();
    }

    fn row<'a>(summary: &'a Value, decision: &str) -> &'a Value {
        summary["pnl"]["by_decision_and_horizon"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["decision"] == decision && r["horizon"] == "30m")
            .unwrap_or_else(|| panic!("no {decision} row in {summary}"))
    }

    #[sqlx::test(migrations = "../signal/migrations")]
    async fn buy_and_shadow_no_buy_are_summarised_per_horizon(pool: PgPool) {
        seed(&pool, 1, "buy", "closed", "100", "101.5").await;
        seed(&pool, 2, "no_buy", "closed", "50", "49").await;
        seed(&pool, 3, "buy", "no_exit_quote", "100", "150").await;
        let summary = summarise(&pool, None, None).await.unwrap();

        let buy = row(&summary, "buy");
        assert_eq!(buy["n"], 1);
        assert_eq!(buy["mean_pnl_pct"], "1.5000");
        assert_eq!(buy["median_pnl_pct"], "1.5000");
        assert_eq!(buy["share_positive_pct"], "100.00");
        let no_buy = row(&summary, "no_buy");
        assert_eq!(no_buy["mean_pnl_pct"], "-2.0000");
        assert_eq!(no_buy["share_positive_pct"], "0.00");

        let side = &summary["pnl"]["buy_vs_no_buy"][0];
        assert_eq!(side["horizon"], "30m");
        assert_eq!(side["buy"]["mean_pnl_pct"], "1.5000");
        assert_eq!(side["no_buy"]["mean_pnl_pct"], "-2.0000");

        assert_eq!(summary["pnl"]["closed_trades"], 2);
        assert_eq!(summary["pnl"]["excluded_trades"]["total"], 1);
        assert_eq!(
            summary["pnl"]["excluded_trades"]["by_status"][0],
            json!({ "status": "no_exit_quote", "n": 1 })
        );
        assert_eq!(summary["observations"]["total"], 3);
        assert_eq!(summary["observations"]["late"], 1);
        assert_eq!(summary["observations"]["late_or_missed_pct"], "33.33");
        let unknown = &summary["release_to_entry"]["by_release_session"][0];
        assert_eq!(unknown["release_session"], "unknown");
        assert_eq!(unknown["median_ms"], "200.0");
    }

    #[sqlx::test(migrations = "../signal/migrations")]
    async fn a_late_observation_is_not_part_of_the_horizon_figures(pool: PgPool) {
        seed(&pool, 1, "buy", "closed", "100", "101").await;
        sqlx::query("UPDATE simulated_trade_observations SET late = true")
            .execute(&pool)
            .await
            .unwrap();
        let summary = summarise(&pool, None, None).await.unwrap();
        assert!(summary["pnl"]["by_decision_and_horizon"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(summary["pnl"]["closed_trades"], 1);
        assert_eq!(summary["observations"]["late"], 1);
    }

    #[sqlx::test(migrations = "../signal/migrations")]
    async fn the_release_session_survives_a_language_twin_merge(pool: PgPool) {
        seed(&pool, 1, "buy", "closed", "100", "101").await;
        sqlx::query(
            r#"INSERT INTO special_events
               (ticker, exchange_mic, event_type, occurred_at, source, source_id, lang, status,
                release_session)
               SELECT 'AAA', 'XSTO', 'quarterly_report', published_at - interval '20 seconds',
                      'mfn', 'english-copy-id', 'en', 'occurred', 'pre_open'
               FROM simulated_trades WHERE trade_id = 1"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let summary = summarise(&pool, None, None).await.unwrap();
        let session = &summary["release_to_entry"]["by_release_session"][0];
        assert_eq!(session["release_session"], "pre_open");
    }

    #[sqlx::test(migrations = "../signal/migrations")]
    async fn the_median_is_the_mean_of_the_two_middle_returns(pool: PgPool) {
        for (id, bid) in [(1, "101"), (2, "102"), (3, "110"), (4, "130")] {
            seed(&pool, id, "buy", "closed", "100", bid).await;
        }
        let summary = summarise(&pool, None, None).await.unwrap();
        assert_eq!(row(&summary, "buy")["median_pnl_pct"], "6.0000");
        assert_eq!(row(&summary, "buy")["mean_pnl_pct"], "10.7500");
    }

    #[sqlx::test(migrations = "../signal/migrations")]
    async fn trades_are_listed_newest_first_with_filters(pool: PgPool) {
        seed(&pool, 1, "buy", "closed", "100", "101").await;
        seed(&pool, 2, "no_buy", "closed", "50", "49").await;
        sqlx::query(
            "INSERT INTO analyst_decisions (news_id, ticker, exchange_mic, lang, published_at, received_at, decided_at, decision, reason, trace_id)
             VALUES ('n2', 'AAA', 'XSTO', 'en', now(), now(), now(), 'no_buy', 'weak guidance', '4bf92f3577b34da6a3ce929d0e0e4736')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let all = fetch_trades(&pool, None, None, None, true, None)
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].trade_id, 2);
        assert_eq!(all[0].reason.as_deref(), Some("weak guidance"));
        assert_eq!(
            all[0].trace_id.as_deref(),
            Some("4bf92f3577b34da6a3ce929d0e0e4736")
        );
        let real = fetch_trades(&pool, None, None, None, false, None)
            .await
            .unwrap();
        assert_eq!(real.len(), 1);
        assert_eq!(real[0].decision, "buy");
        let none = fetch_trades(&pool, Some("BBB"), None, None, true, Some(10))
            .await
            .unwrap();
        assert!(none.is_empty());
        let one = fetch_trades(&pool, None, Some("no_buy"), None, true, Some(1))
            .await
            .unwrap();
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn since_accepts_dates_and_instants() {
        assert!(parse_since("2026-11-01").is_ok());
        assert!(parse_since("2026-11-01T09:00:00+01:00").is_ok());
        assert!(parse_since("yesterday").is_err());
    }
}
