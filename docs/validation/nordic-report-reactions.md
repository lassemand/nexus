# Nordic report reactions: validation for GomSpace (GOMX, FNSE)

NEX-165. A wrong percentage looks exactly like a right one, so the pipeline was checked against sources
that do not come from our own database.

## Status: done (part A and part B)

| | What | Status |
|---|---|---|
| A | The **logic** (session rule, reaction and base day, holiday calendar, corrections, re-sends, moves) replayed on all 38 real GomSpace reports with **independent prices and an independent trading calendar** | **Done**, no mismatch. Results below |
| B | The **production data**: `special_events` / `daily_bars` in the `backtest` database compared with the independent closes | **Done** 2026-10-26 on production (0.2.3). Dates 38 of 38; closes identical for 30, a constant adjustment factor for the 8 before 2018-11-16; moves agree for all 38. See [B](#b-production-vs-independent) |

Part A uses the independent prices *as* the price input, so it says nothing about whether Saxo's stored closes are
right. Part B checks exactly that, against the production database.

## Sources

- **Release times and classification:** MFN's public JSON feed `https://mfn.se/all/a/gomspace-group.json` (358 items,
  fetched 2026-10-25), classified by the real `chronicle_lib::nordic::classify` and turned into events by the real
  `release_event`. 42 events: 31 quarterly, 9 annual and 2 explicit corrections (plus 2 re-sends that signal turns into
  corrections, see A2).
- **Independent prices:** Yahoo Finance `GOMX.ST` daily bars, `https://query1.finance.yahoo.com/v8/finance/chart/GOMX.ST`
  (2,606 bars, 2016-06-16 to 2026-10-23, fetched 2026-10-25). Nasdaq Nordic's own price page could not be used (the old
  microsite redirects and `api.nasdaq.com` returned nothing). Yahoo reports no split or dividend events for the
  instrument, and its `close` equals `adjclose` for every bar.
- **Independent calendar:** Yahoo's own list of trading days, not signal's `trading_holidays` table.
- **Replay:** the events were fed through signal's real consumer path (`reports::parse` and `reports::handle`) into a
  throwaway, fully migrated Postgres, the Yahoo closes were stored as `daily_bars`, and `refresh_all` computed the
  reactions. So the stored-report rules (twin merge, re-send to correction, correction links) and the reaction code
  are the production code.

## A1. Sample of 12 of the 38 reports

All 38 are `data_status = 'ok'`. The sample has the oldest report, 5 annual reports, the 2025-08-27 report (which has a
correction), a re-send, the largest move and the latest report. Closes are Yahoo's, rounded to 4 decimals. Session rule:
before 09:00 = pre-open, 09:00 up to the close (17:30, or 13:00 on half days) = intraday, otherwise after the close and
the reaction is on the next trading day.

| Released (Stockholm) | Report | Session | Reaction day | Base day | Independent base close | Independent reaction close | Independent move |
|---|---|---|---|---|---|---|---|
| 2016-08-22 17:04 | quarterly (oldest available (Q2 2016, released 17:04)) | intraday | 2016-08-22 | 2016-08-19 | 18.8836 | 17.4180 | -7.76 % |
| 2017-02-28 08:00 | annual | pre-open | 2017-02-28 | 2017-02-27 | 19.6164 | 19.7291 | +0.57 % |
| 2019-02-28 08:02 | annual (re-send the same morning became a correction) | pre-open | 2019-02-28 | 2019-02-27 | 13.3102 | 12.7971 | -3.85 % |
| 2020-04-23 08:00 | quarterly | pre-open | 2020-04-23 | 2020-04-22 | 5.2441 | 5.5384 | +5.61 % |
| 2021-01-28 08:00 | annual | pre-open | 2021-01-28 | 2021-01-27 | 11.2427 | 10.8956 | -3.09 % |
| 2022-05-02 08:00 | quarterly | pre-open | 2022-05-02 | 2022-04-29 | 8.5867 | 8.9338 | +4.04 % |
| 2023-08-22 08:00 | quarterly | pre-open | 2023-08-22 | 2023-08-21 | 1.3150 | 1.2050 | -8.37 % |
| 2024-02-27 08:00 | annual | pre-open | 2024-02-27 | 2024-02-26 | 5.2400 | 5.0400 | -3.82 % |
| 2025-05-07 08:00 | quarterly (largest move in the data) | pre-open | 2025-05-07 | 2025-05-06 | 7.7000 | 9.4500 | +22.73 % |
| 2025-08-27 08:00 | quarterly (has a correction (09:23)) | pre-open | 2025-08-27 | 2025-08-26 | 11.7800 | 13.1000 | +11.21 % |
| 2026-02-19 08:00 | annual | pre-open | 2026-02-19 | 2026-02-18 | 21.1500 | 22.2000 | +4.96 % |
| 2026-08-26 08:00 | quarterly | pre-open | 2026-08-26 | 2026-08-25 | 11.1500 | 11.3500 | +1.79 % |

The production columns (`base_close`, `reaction_close`, `move_pct` from `special_events`, and the differences) are
produced by Q1 below for **all 38** reports at once.

### Cross-check of every report against Yahoo's calendar

For each of the 38 reports the reaction day and base day were derived a second time, in Python, from Yahoo's list of
trading days and `zoneinfo("Europe/Stockholm")` (release day if it is a trading day and the local time is before 17:30,
otherwise the next trading day; base = the previous trading day). Result: **0 mismatches in 38**, for both the reaction
day and the base day. That covers every DST and holiday case in ten years of GomSpace reports, including reports on
both sides of the March and October clock changes.

What the real data does **not** exercise: GomSpace has 37 pre-open reports and one intraday report (the 2016 one,
17:04). There is no real after-close, weekend, holiday or half-day release, so those rules are covered only by the unit
tests in `signal/src/reactions.rs` (Friday after close, Saturday, 2026-04-02 18:00 before Easter, 2026-12-24 14:00).

## A2. Sanity checks (SQL and result)

Results are from the **replay database** (the 42 real GomSpace events). Run the same queries on `backtest` for the
production result.

```sql
-- S1: look-ahead the wrong way round (reaction before the release date)
SELECT count(*) FROM special_events
WHERE source = 'mfn' AND status = 'occurred' AND event_type IN ('quarterly_report','annual_report')
  AND reaction_date < (occurred_at AT TIME ZONE 'Europe/Stockholm')::date;
-- result: 0

-- S2a: corrections counted as reports
SELECT count(*) FROM special_events
WHERE source = 'mfn' AND status = 'occurred' AND event_type IN ('quarterly_report','annual_report') AND is_correction;
-- result: 0

-- S2b: correction rows with no report pointing at them through superseded_by
SELECT to_char(c.occurred_at AT TIME ZONE 'Europe/Stockholm','YYYY-MM-DD HH24:MI') AS occurred_local,
       left(c.description, 70) AS description
FROM special_events c
WHERE c.source = 'mfn' AND c.event_type = 'report_correction'
  AND NOT EXISTS (SELECT 1 FROM special_events o WHERE o.superseded_by = c.id);
-- result: 1 row: 2025-06-25 21:40, "GomSpace A/S: Correction: GomSpace signs a 19.5 MEUR contract with a E..." (finding F1)

-- S3: needs_review (|move| > 40 %)
SELECT count(*) FROM special_events
WHERE source = 'mfn' AND status = 'occurred' AND event_type IN ('quarterly_report','annual_report') AND needs_review;
-- result: 0

-- S4: rows per data_status
SELECT coalesce(data_status, '(null)') AS data_status, count(*) FROM special_events
WHERE source = 'mfn' AND status = 'occurred' AND event_type IN ('quarterly_report','annual_report') GROUP BY 1;
-- result: ok = 38

-- S5: the five largest absolute moves
SELECT to_char(occurred_at AT TIME ZONE 'Europe/Stockholm','YYYY-MM-DD') AS released, move_pct FROM special_events
WHERE move_pct IS NOT NULL ORDER BY abs(move_pct) DESC LIMIT 5;
-- result: 2025-05-07 +22.7273, 2025-08-27 +11.2054, 2024-10-31 +8.7883, 2023-08-22 -8.3650, 2019-11-29 -8.0182
```

There are 4 `report_correction` rows in total. Two are explicit "Correction:" notices (2025-08-27 09:23 and the contract
notice 2025-06-25) and two are same-day re-sends that signal turned into corrections (2019-02-28 08:30 and 2019-08-30
10:08). The originals of three of them (2019-02-28 08:02, 2019-08-30 08:00, 2025-08-27 08:00) have `superseded_by`
set. The market-moving release is always the earliest one, and no correction is counted as a second report.

## A3. Are Saxo's daily bars split-adjusted?

**No: Saxo returns unadjusted traded closes** (established in [B](#b-production-vs-independent), on production data
from the live chart API `v3`; NEX-183). The text below was written before production data was available and is kept
for the reasoning about why moves still agree.

About the *independent* series: Yahoo lists no split or dividend event for GOMX.ST. Whether its history is nevertheless
adjusted for older share issues **could not be established from the data alone**. What is visible: the first bar
(2016-06-16) closes at SEK 4.90, the series reaches about SEK 60 in late 2017, is between SEK 1.2 and 2.9 during 2023, and
is above SEK 20 in 2026. I did not compare these with raw historical prices, so no conclusion is drawn about
adjustment. What matters for the check: if one series is adjusted and the other is not, their closes differ by a
constant factor for older reports, while the **move %** of a report is the same in both unless an issue falls between its
base day and its reaction day. Q1 therefore compares closes (the 0.5 % rule) **and** moves (`independent_move_pct`).
A constant close ratio with agreeing moves means an adjustment difference, not a pipeline error. `needs_review`
(|move| > 40 %) is the safety net for an issue that falls exactly between two compared days. Look at the 2016 to about
2020 rows first.

## B. Production vs independent

Run on 2026-10-26 against `backtest` on 0.2.3, after `saxo-stream` had published 2,605 GOMX daily bars from Saxo's live
chart API `v3` (2016-06-16 to 2026-10-23; Yahoo has 2,606 for the same range). All 38 reports had
`data_status = ok`. Query [Q1](#q1-production-vs-independent-run-this-on-backtest), unchanged.

| | Result |
|---|---|
| Reaction day and base day (`dates_ok`) | **38 of 38** match the independent calendar |
| Closes, reports from 2019-02-28 on (30) | **Identical** to Yahoo (difference 0.00 %, Saxo has more decimals) |
| Closes, the 8 reports from 2016-08-22 to 2018-11-06 | Both base and reaction close **exactly 33.86 % higher** than Yahoo (ratio 1.3386) |
| Move % | **Agrees for all 38**, to 3-4 decimals (largest difference 0.0015 points, from Yahoo's rounding to 4 decimals) |

The 8 older rows (`closes_ok = false`):

| Released (Stockholm) | Saxo base | Yahoo base | Saxo reaction | Yahoo reaction | Saxo move | Yahoo move |
|---|---|---|---|---|---|---|
| 2016-08-22 17:04 | 25.2773 | 18.8836 | 23.3154 | 17.4180 | -7.7612 % | -7.7612 % |
| 2017-02-28 08:00 | 26.2582 | 19.6164 | 26.4091 | 19.7291 | +0.5747 % | +0.5745 % |
| 2017-05-31 08:00 | 44.1409 | 32.9758 | 45.4613 | 33.9623 | +2.9914 % | +2.9916 % |
| 2017-08-31 08:00 | 46.4045 | 34.6669 | 45.6500 | 34.1032 | -1.6260 % | -1.6260 % |
| 2017-11-27 08:00 | 79.2272 | 59.1874 | 80.9249 | 60.4557 | +2.1428 % | +2.1429 % |
| 2018-05-24 08:00 | 47.9136 | 35.7943 | 51.3091 | 38.3309 | +7.0866 % | +7.0866 % |
| 2018-08-31 08:00 | 31.5400 | 23.5622 | 29.9932 | 22.4067 | -4.9043 % | -4.9040 % |
| 2018-11-06 08:00 | 24.3341 | 18.1790 | 22.5232 | 16.8261 | -7.4418 % | -7.4421 % |

**Where the factor comes from.** Comparing the daily closes around the change, the ratio is 1.3386 up to
2018-11-15 and 1.0000 from 2018-11-16:

| Date | Saxo close | Yahoo close | Saxo / Yahoo |
|---|---|---|---|
| 2018-11-15 | 17.5809 | 13.1340 | 1.3386 |
| 2018-11-16 | 15.0004 | 15.0004 | 1.0000 |

Saxo falls 14.7 % that day, as traded prices do on an ex-date; Yahoo rises 14.2 %, because it divides every close
before 2018-11-16 by 1.3386. So **Saxo's bars are unadjusted traded closes, and Yahoo's history is adjusted for a
corporate action effective 2018-11-16.** Which action it was (a share issue, presumably) was not checked against an
announcement. No GomSpace report has its base day before and its reaction day on or after 2018-11-16, so no move is
affected.

**Sanity checks on production** (the queries of A2, on `backtest`): S1 look-ahead **0**; S2a corrections counted as
reports **0**; S2b one correction without an original report, the 2025-06-25 contract notice (F1); S3 `needs_review`
**0**; S4 `ok` = **38**; S5 largest moves 2025-05-07 +22.7273, 2025-08-27 +11.2054, 2024-10-31 +8.7883,
2023-08-22 -8.3650, 2019-11-29 -8.0181. Identical to the replay (S5's last digit differs by rounding).

## Findings

- **F1 (follow-up issue, minor).** chronicle publishes *any* item whose title starts with "Correction:" as a
  `report_correction`, also when it corrects a non-report (2025-06-25, the 19.5 MEUR contract). The row has no original
  report and is excluded from the dashboard and from reactions, so it does no harm, but it breaks the invariant that
  every `report_correction` links to a report. Not patched here (verification only).
- **F2 (resolved).** Saxo's daily bars are **not** adjusted for corporate actions; Yahoo's are (B). A report's move %
  is unaffected unless its base day and reaction day straddle an ex-date.
- **F5 (follow-up issue).** Because Saxo's closes are unadjusted, a report whose base day is before an ex-date and whose
  reaction day is on or after it would show the mechanical ex-date drop as part of its move (GOMX: -14.7 % on
  2018-11-16, below the 40 % `needs_review` threshold, so it would pass unflagged). No GomSpace report does, but the
  reaction code should either adjust for, or flag, a corporate action between the two days.
- **F3.** GomSpace has no real after-close, weekend, holiday or half-day report, so those rules are verified by unit
  tests only. Repeat this check for a company that has them.
- **F4.** Prices around 2023-05-26 are round numbers (1.5000 to 1.4250, a move of exactly -5.0000 %): a thin stock with
  a coarse tick size. Not an error, but such moves are a function of the tick.

## Pass criteria

| Criterion | Result |
|---|---|
| At least 8 reports, with an annual report, 2025-08-27 and the oldest | 12 in the table, 38 in the full check |
| Reaction date matches the session rule (independent calendar) | **Pass**, 38 of 38 |
| Base date is the previous trading day | **Pass**, 38 of 38 |
| Closes match an independent source within 0.5 % | **Pass for 30 of 38** (identical). The other 8 differ by a constant factor of 1.3386 from Yahoo's adjustment for the 2018-11-16 corporate action; their moves agree, so this is an adjustment difference, not a pipeline error (B) |
| No look-ahead (S1), no corrections counted as reports (S2a), `needs_review` explained (S3), counts per status (S4) | **Pass** on the replay and on `backtest` |
| Saxo split adjustment documented | **Documented**: unadjusted (A3, B) |

## Q1: production vs independent (run this on `backtest`)

One row per report. It is a `LEFT JOIN`, so a report missing from production shows NULLs instead of disappearing.
Tolerance: closes within 0.5 % (closing auction vs last trade), dates exact. The independent values are Yahoo's closes
rounded to 4 decimals. Run it in Grafana Explore on the *Nexus Postgres* datasource, or with `psql`, and paste the
result (at least the rows where `dates_ok` or `closes_ok` is false) into the PR.

```sql
WITH independent (occurred_at, reaction_date, base_date, reaction_close, base_close) AS (VALUES
 ('2016-08-22T15:04:12Z'::timestamptz, DATE '2016-08-22', DATE '2016-08-19', 17.4180::numeric, 18.8836::numeric),
 ('2017-02-28T07:00:00Z'::timestamptz, DATE '2017-02-28', DATE '2017-02-27', 19.7291::numeric, 19.6164::numeric),
 ('2017-05-31T06:00:00Z'::timestamptz, DATE '2017-05-31', DATE '2017-05-30', 33.9623::numeric, 32.9758::numeric),
 ('2017-08-31T06:00:00Z'::timestamptz, DATE '2017-08-31', DATE '2017-08-30', 34.1032::numeric, 34.6669::numeric),
 ('2017-11-27T07:00:00Z'::timestamptz, DATE '2017-11-27', DATE '2017-11-24', 60.4557::numeric, 59.1874::numeric),
 ('2018-05-24T06:00:00Z'::timestamptz, DATE '2018-05-24', DATE '2018-05-23', 38.3309::numeric, 35.7943::numeric),
 ('2018-08-31T06:00:00Z'::timestamptz, DATE '2018-08-31', DATE '2018-08-30', 22.4067::numeric, 23.5622::numeric),
 ('2018-11-06T07:00:00Z'::timestamptz, DATE '2018-11-06', DATE '2018-11-05', 16.8261::numeric, 18.1790::numeric),
 ('2019-02-28T07:02:00Z'::timestamptz, DATE '2019-02-28', DATE '2019-02-27', 12.7971::numeric, 13.3102::numeric),
 ('2019-05-29T06:00:00Z'::timestamptz, DATE '2019-05-29', DATE '2019-05-28', 9.5375::numeric, 10.2920::numeric),
 ('2019-08-30T06:00:00Z'::timestamptz, DATE '2019-08-30', DATE '2019-08-29', 8.1189::numeric, 7.6058::numeric),
 ('2019-11-29T07:00:00Z'::timestamptz, DATE '2019-11-29', DATE '2019-11-28', 9.1753::numeric, 9.9751::numeric),
 ('2020-02-28T07:00:00Z'::timestamptz, DATE '2020-02-28', DATE '2020-02-27', 6.7758::numeric, 6.3759::numeric),
 ('2020-04-23T06:00:00Z'::timestamptz, DATE '2020-04-23', DATE '2020-04-22', 5.5384::numeric, 5.2441::numeric),
 ('2020-07-16T06:00:00Z'::timestamptz, DATE '2020-07-16', DATE '2020-07-15', 6.2627::numeric, 6.4287::numeric),
 ('2020-10-22T06:00:08Z'::timestamptz, DATE '2020-10-22', DATE '2020-10-21', 7.6964::numeric, 7.2285::numeric),
 ('2021-01-28T07:00:19Z'::timestamptz, DATE '2021-01-28', DATE '2021-01-27', 10.8956::numeric, 11.2427::numeric),
 ('2021-04-26T06:00:00Z'::timestamptz, DATE '2021-04-26', DATE '2021-04-23', 11.8916::numeric, 12.7518::numeric),
 ('2021-07-19T06:00:00Z'::timestamptz, DATE '2021-07-19', DATE '2021-07-16', 13.8535::numeric, 14.6835::numeric),
 ('2021-10-25T06:00:00Z'::timestamptz, DATE '2021-10-25', DATE '2021-10-22', 9.7336::numeric, 9.8544::numeric),
 ('2022-02-01T07:00:00Z'::timestamptz, DATE '2022-02-01', DATE '2022-01-31', 7.8473::numeric, 7.8624::numeric),
 ('2022-05-02T06:00:00Z'::timestamptz, DATE '2022-05-02', DATE '2022-04-29', 8.9338::numeric, 8.5867::numeric),
 ('2022-07-25T06:00:00Z'::timestamptz, DATE '2022-07-25', DATE '2022-07-22', 6.6475::numeric, 6.7758::numeric),
 ('2022-11-28T07:00:00Z'::timestamptz, DATE '2022-11-28', DATE '2022-11-25', 4.3839::numeric, 4.6782::numeric),
 ('2023-02-07T07:00:00Z'::timestamptz, DATE '2023-02-07', DATE '2023-02-06', 2.7503::numeric, 2.9088::numeric),
 ('2023-05-26T06:00:00Z'::timestamptz, DATE '2023-05-26', DATE '2023-05-25', 1.4250::numeric, 1.5000::numeric),
 ('2023-08-22T06:00:46Z'::timestamptz, DATE '2023-08-22', DATE '2023-08-21', 1.2050::numeric, 1.3150::numeric),
 ('2023-11-16T07:00:00Z'::timestamptz, DATE '2023-11-16', DATE '2023-11-15', 1.6800::numeric, 1.7200::numeric),
 ('2024-02-27T07:00:00Z'::timestamptz, DATE '2024-02-27', DATE '2024-02-26', 5.0400::numeric, 5.2400::numeric),
 ('2024-05-17T06:00:40Z'::timestamptz, DATE '2024-05-17', DATE '2024-05-16', 4.2900::numeric, 4.4050::numeric),
 ('2024-08-23T06:00:51Z'::timestamptz, DATE '2024-08-23', DATE '2024-08-22', 5.4000::numeric, 5.4600::numeric),
 ('2024-10-31T07:00:00Z'::timestamptz, DATE '2024-10-31', DATE '2024-10-30', 4.0850::numeric, 3.7550::numeric),
 ('2025-05-07T06:00:00Z'::timestamptz, DATE '2025-05-07', DATE '2025-05-06', 9.4500::numeric, 7.7000::numeric),
 ('2025-08-27T06:00:00Z'::timestamptz, DATE '2025-08-27', DATE '2025-08-26', 13.1000::numeric, 11.7800::numeric),
 ('2025-11-06T07:00:00Z'::timestamptz, DATE '2025-11-06', DATE '2025-11-05', 15.5800::numeric, 16.6200::numeric),
 ('2026-02-19T07:00:25Z'::timestamptz, DATE '2026-02-19', DATE '2026-02-18', 22.2000::numeric, 21.1500::numeric),
 ('2026-05-07T06:00:00Z'::timestamptz, DATE '2026-05-07', DATE '2026-05-06', 18.5400::numeric, 19.1400::numeric),
 ('2026-08-26T06:00:00Z'::timestamptz, DATE '2026-08-26', DATE '2026-08-25', 11.3500::numeric, 11.1500::numeric)
)
SELECT to_char(i.occurred_at AT TIME ZONE 'Europe/Stockholm', 'YYYY-MM-DD HH24:MI') AS released_local,
       e.release_session,
       e.reaction_date, i.reaction_date AS independent_reaction_date,
       e.base_date, i.base_date AS independent_base_date,
       e.base_close, i.base_close AS independent_base_close,
       round((e.base_close / i.base_close - 1) * 100, 2) AS base_diff_pct,
       e.reaction_close, i.reaction_close AS independent_reaction_close,
       round((e.reaction_close / i.reaction_close - 1) * 100, 2) AS reaction_diff_pct,
       e.move_pct, round((i.reaction_close / i.base_close - 1) * 100, 4) AS independent_move_pct,
       e.data_status,
       (e.reaction_date = i.reaction_date AND e.base_date = i.base_date) AS dates_ok,
       (abs(e.base_close / i.base_close - 1) <= 0.005
        AND abs(e.reaction_close / i.reaction_close - 1) <= 0.005) AS closes_ok
FROM independent i
LEFT JOIN special_events e
  ON e.ticker = 'GOMX' AND e.exchange_mic = 'FNSE' AND e.source = 'mfn'
 AND e.status = 'occurred' AND NOT e.is_correction AND e.occurred_at = i.occurred_at
ORDER BY i.occurred_at
```

To repeat this for another company, replay its MFN feed the same way and generate the same query from that company's
independent closes.
