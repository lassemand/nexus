-- View: ticker_event_consistency
--
-- Per-ticker consistency metrics computed over qualifying events
-- (post_ar_1d >= 0.05 — events where the stock moved at least 5% on day 1).
-- Small-move events are excluded; a < 5% move is indistinguishable from noise.
--
-- pre_event_suspicion is computed inline as car_20d * mean_abvol_20d (CAR × ABVOL).
-- When NEX-56 adds a dedicated column to event_signals, update this view to
-- reference es.pre_event_suspicion directly.

CREATE VIEW ticker_event_consistency AS
SELECT
    es.ticker,
    COUNT(*)                                                        AS event_count,
    ROUND(AVG(CASE WHEN es.post_ar_1d > 0.08 THEN 1.0 ELSE 0.0 END)::numeric, 3)
                                                                    AS move_rate,
    ROUND(AVG(es.mean_abvol_20d)::numeric, 3)                      AS avg_abvol_before_event,
    ROUND(AVG(COALESCE(es.car_20d, 0) * COALESCE(es.mean_abvol_20d, 0))::numeric, 3)
                                                                    AS avg_suspicion,
    ROUND(AVG(CASE WHEN EXISTS (
        SELECT 1 FROM insider_filings f
        WHERE f.ticker = es.ticker
          AND f.transaction_code = 'P'
          AND f.transaction_date BETWEEN es.event_date - INTERVAL '20 days'
                                     AND es.event_date
    ) THEN 1.0 ELSE 0.0 END)::numeric, 3)                          AS insider_purchase_rate,
    ROUND((
        AVG(CASE WHEN es.post_ar_1d > 0.08 THEN 1.0 ELSE 0.0 END) *
        COALESCE(AVG(es.mean_abvol_20d), 0) *
        (1 + AVG(CASE WHEN EXISTS (
            SELECT 1 FROM insider_filings f
            WHERE f.ticker = es.ticker
              AND f.transaction_code = 'P'
              AND f.transaction_date BETWEEN es.event_date - INTERVAL '20 days'
                                         AND es.event_date
        ) THEN 1.0 ELSE 0.0 END))
    )::numeric, 4)                                                  AS consistency_score
FROM event_signals es
WHERE es.post_ar_1d >= 0.05
  AND es.mean_abvol_20d IS NOT NULL
GROUP BY es.ticker
HAVING COUNT(*) >= 2
ORDER BY consistency_score DESC;
