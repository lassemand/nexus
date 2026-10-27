# Novo Nordisk (NOVO-B, XCSE): reports, upcoming date and reactions validated against independent closes

NEX-192. A wrong percentage looks exactly like a right one, so the end result for Novo Nordisk was checked against sources that do not come from our own pipeline. It mirrors the GomSpace validation in [`nordic-report-reactions.md`](nordic-report-reactions.md) (NEX-165).

## Status: done on production, 2026-10-27, with findings

| | What | Result |
|---|---|---|
| Reports | Every published report vs MFN's calendar history, 2011-2026 | **2020-2026: 27 of 27 published exactly once.** 2024-2026: 11 of 11. **Before 2020: 21 MFN results items are not classified as reports** (NEX-198). No share-repurchase or PDMR item is a report |
| Upcoming | Q3 2026 scheduled row | **Present**: `scheduled`, 2026-11-04 18:35 Copenhagen. Today is 2026-10-27, so the change to `occurred` after 2026-11-04 could not be checked |
| Reactions | 31 of 31 `ok` reports: reaction day, base day and session rebuilt from an independent calendar and `Europe/Copenhagen`; closes compared with Yahoo | **0 date or session mismatches; closes identical (difference 0.000 %) for all 31; moves identical** |
| Split | Saxo bars around 2023-09-13 | **Saxo's closes are split-adjusted** (673.70 on the 13th after 681.50 on the 12th). No Novo report straddles the split |
| Sanity SQL | look-ahead, corrections, `needs_review`, counts | S1 **0**, S2a **0**, S3 **0**; `ok` 31, `calendar_unknown` 9 (all before 2015) |
| Dashboard | "Nordic Reports" | Novo is in the dropdown with its past reports and the 2026-11-04 date (checked by running the dashboard's own SQL, see [Dashboard](#dashboard)); Stockholm-only wording and a doubled label are follow-ups (NEX-200) |

## Sources and method

- **Our data:** production signal database `backtest`, read-only queries (`SET TRANSACTION READ ONLY`): `special_events`, `daily_bars`, `companies`, `exchanges`, `trading_holidays`. The queries are at the end.
- **Independent prices:** Yahoo Finance `NOVO-B.CO` daily bars, `https://query1.finance.yahoo.com/v8/finance/chart/NOVO-B.CO` (4,203 bars, 2010-01-04 to 2026-10-27, fetched 2026-10-27, `interval=1d`, `events=split|div`). Yahoo reports splits 5:1 on 2014-01-02 and 2:1 on 2023-09-13 and its `close` is split-adjusted. Nasdaq Nordic's own price pages were not tried this time (NEX-165 found them unusable without a browser), so **no unadjusted traded close from the exchange was available**. Yahoo and Saxo may share an upstream; the check shows that our pipeline reproduces an outside series, not that either is the exchange's official close.
- **Independent calendar:** Yahoo's own list of trading days, not `trading_holidays`.
- **Independent time zone rule:** Python `zoneinfo("Europe/Copenhagen")`; session: before 09:00 pre-open, 09:00 up to 17:00 intraday, 17:00 or later after the close (reaction on the next trading day); a non-trading day goes to the next trading day.
- **Reports and the financial calendar:** MFN's complete news feed `https://mfn.se/all/a/novo-nordisk.json` (2,002 items, 2011-02-02 to 2026-10-27, classified with the real `chronicle_lib::nordic::classify`) and MFN's calendar page `https://mfn.se/a/novo-nordisk` (62 report rows since 2011). Novo's investor site was not used.

## 1. Reports

All `source = 'mfn'`, `status = 'occurred'`, `ticker = 'NOVO-B'`, `exchange_mic = 'XCSE'` quarterly and annual reports (40 rows, query R1), Copenhagen time. `calendar_unknown` rows are from before 2015: `trading_holidays` for DK and Saxo's bars start in 2015, so no reaction is computed (by design, never guessed).

| # | Released (Copenhagen) | Kind | Period | Lang | Session | Reaction day | Status | Headline |
|---|---|---|---|---|---|---|---|---|
| 265 | 2011-02-02 08:29 | annual | 2010 | en |  |  | calendar_unknown | Financial statement for 2010 |
| 264 | 2011-10-27 07:29 | quarterly |  | en |  |  | calendar_unknown | Novo Nordisk A/S: Interim financial report for the period 1 January 20… |
| 262 | 2012-02-02 07:29 | annual | 2011 | en |  |  | calendar_unknown | Novo Nordisk A/S: Financial Statement - Novo Nordisk increased operati… |
| 261 | 2012-04-27 07:30 | quarterly |  | da |  |  | calendar_unknown | Novo Nordisk A/S: Resultat af primær drift steg med 18% i første kvart… |
| 259 | 2012-08-09 07:39 | quarterly |  | da |  |  | calendar_unknown | Novo Nordisk A/S: Novo Nordisks resultat af primær drift steg med 31% … |
| 258 | 2013-05-01 07:30 | quarterly | 2013-Q1 | en |  |  | calendar_unknown | Novo Nordisk A/S increased operating profit by 18% in the first quarte… |
| 256 | 2013-10-31 07:31 | quarterly |  | da |  |  | calendar_unknown | Novo Nordisk A/S: Delårsrapport for perioden 1. januar 2013 til 30. se… |
| 255 | 2014-05-01 07:30 | quarterly |  | da |  |  | calendar_unknown | Novo Nordisk A/S: Novo Nordisks resultat af primær drift opgjort i lok… |
| 253 | 2014-08-07 07:30 | quarterly |  | da |  |  | calendar_unknown | Novo Nordisks resultat af primær drift opgjort i lokale valutaer steg … |
| 252 | 2015-04-30 07:30 | quarterly |  | da | pre_open | 2015-04-30 | ok | Novo Nordisks resultat af primær drift i kroner steg med 73% i første … |
| 250 | 2015-08-06 07:31 | quarterly |  | da | pre_open | 2015-08-06 | ok | Novo Nordisks resultat af primær drift opgjort i kroner steg med 57% i… |
| 249 | 2016-04-29 07:30 | quarterly | 2016-Q1 | en | pre_open | 2016-04-29 | ok | Novo Nordisk increased adjusted operating profit by 10% in local curre… |
| 248 | 2019-08-09 07:31 | quarterly | 2019-Q2 | en | pre_open | 2019-08-09 | ok | Novo Nordisk's operating profit increased by 12% in Danish kroner and … |
| 247 | 2020-02-05 07:31 | annual | 2019 | en | pre_open | 2020-02-05 | ok | Novo Nordisk's operating profit increased by 11% in Danish kroner and … |
| 246 | 2020-05-06 07:31 | quarterly | 2020-Q1 | en | pre_open | 2020-05-06 | ok | Novo Nordisk's sales increased by 16% in Danish kroner and by 14% at c… |
| 245 | 2020-08-06 07:30 | quarterly | 2020-Q2 | en | pre_open | 2020-08-06 | ok | Novo Nordisk's operating profit increased by 9% in Danish kroner and b… |
| 244 | 2020-10-30 07:30 | quarterly | 2020-Q3 | en | pre_open | 2020-10-30 | ok | Novo Nordisk's operating profit increased by 6% in Danish kroner and b… |
| 243 | 2021-02-03 07:30 | annual | 2020 | en | pre_open | 2021-02-03 | ok | Novo Nordisk's net profit increased by 8% in 2020 |
| 241 | 2021-05-05 07:30 | quarterly | 2021-Q1 | en | pre_open | 2021-05-05 | ok | Novo Nordisk's net profit increased by 6% in the first three months of… |
| 240 | 2021-08-04 16:24 | quarterly | 2021-Q2 | en | intraday | 2021-08-04 | ok | Novo Nordisk's net profit increased by 10% in the first six months of … |
| 238 | 2021-11-03 07:30 | quarterly | 2021-Q3 | en | pre_open | 2021-11-03 | ok | Novo Nordisk's net profit increased by 12% in the first nine months of… |
| 237 | 2022-02-02 07:30 | annual | 2021 | en | pre_open | 2022-02-02 | ok | Novo Nordisk's sales increased by 11% in Danish kroner and by 14% at c… |
| 236 | 2022-04-29 08:39 | quarterly | 2022-Q1 | en | pre_open | 2022-04-29 | ok | Novo Nordisk's sales increased by 24% in Danish kroner and by 18% at c… |
| 234 | 2022-08-03 16:36 | quarterly | 2022-Q2 | en | intraday | 2022-08-03 | ok | Novo Nordisk's sales increased by 25% in Danish kroner and by 16% at c… |
| 233 | 2022-11-02 07:30 | quarterly | 2022-Q3 | en | pre_open | 2022-11-02 | ok | Novo Nordisk's sales increased by 26% in Danish kroner and by 16% at c… |
| 232 | 2023-02-01 07:30 | annual | 2022 | en | pre_open | 2023-02-01 | ok | Novo Nordisk's sales increased by 26% in Danish kroner and by 16% at c… |
| 231 | 2023-05-04 07:31 | quarterly | 2023-Q1 | en | pre_open | 2023-05-04 | ok | Novo Nordisk's sales increased by 27% in Danish kroner and by 25% at c… |
| 230 | 2023-08-10 07:30 | quarterly | 2023-Q2 | en | pre_open | 2023-08-10 | ok | Novo Nordisk's sales increased by 29% in Danish kroner and by 30% at c… |
| 229 | 2023-11-02 07:30 | quarterly | 2023-Q3 | en | pre_open | 2023-11-02 | ok | Novo Nordisk's sales increased by 29% in Danish kroner and by 33% at c… |
| 228 | 2024-01-31 07:30 | annual | 2023 | en | pre_open | 2024-01-31 | ok | Novo Nordisk's sales increased by 31% in Danish kroner and by 36% at c… |
| 227 | 2024-05-02 07:30 | quarterly | 2024-Q1 | en | pre_open | 2024-05-02 | ok | Novo Nordisk's sales increased by 22% in Danish kroner and by 24% at c… |
| 226 | 2024-08-07 07:31 | quarterly | 2024-Q2 | en | pre_open | 2024-08-07 | ok | Novo Nordisk's sales increased by 24% in Danish kroner and by 25% at c… |
| 225 | 2024-11-06 07:30 | quarterly | 2024-Q3 | en | pre_open | 2024-11-06 | ok | Novo Nordisk's sales increased by 23% in Danish kroner and by 24% at c… |
| 224 | 2025-02-05 07:30 | annual | 2024 | en | pre_open | 2025-02-05 | ok | Novo Nordisk's sales increased by 25% in Danish kroner and by 26% at c… |
| 223 | 2025-05-07 07:30 | quarterly | 2025-Q1 | en | pre_open | 2025-05-07 | ok | Novo Nordisk's sales increased by 19% in Danish kroner and by 18% at c… |
| 222 | 2025-08-06 07:30 | quarterly | 2025-Q2 | en | pre_open | 2025-08-06 | ok | Novo Nordisk's sales increased by 16% in Danish kroner and by 18% at c… |
| 221 | 2025-11-05 07:30 | quarterly | 2025-Q3 | en | pre_open | 2025-11-05 | ok | Novo Nordisk's sales increased by 12% in Danish kroner and by 15% at C… |
| 220 | 2026-02-03 17:38 | annual | 2025 | en | after_close | 2026-02-04 | ok | Novo Nordisk's sales increased by 6% in Danish kroner and by 10% at co… |
| 219 | 2026-05-06 07:31 | quarterly | 2026-Q1 | en | pre_open | 2026-05-06 | ok | Novo Nordisk's adjusted operating profit reached DKK 32,858 million in… |
| 218 | 2026-08-04 18:37 | quarterly | 2026-Q2 | en | after_close | 2026-08-05 | ok | Novo Nordisk reports adjusted operating profit of DKK 33,389 million f… |

No row is a share-repurchase, "Trading in Novo Nordisk shares by board members, …" (PDMR), voting-rights or major-shareholder announcement (query S2c: 0). The 8 `report_correction` rows (not in the table): 5 are the Danish/English twin of a 2011-2015 release published 23-30 s apart (NEX-199), 2 are real same-day re-sends (2021-08-04 16:41, 2022-04-29 08:47), 1 is "Correction: Resolutions from the Annual General Meeting" (2021-03-25, NEX-184).

### Comparison with MFN's financial calendar, 2024-2026 (required)

| Calendar date | Calendar label | Published row (Copenhagen) | Times published | Result |
|---|---|---|---|---|
| 2024-01-31 | Bokslutskommuniké 2023 | 2024-01-31 07:30 | 1 | **Pass** |
| 2024-05-02 | Kvartalsrapport 2024-Q1 | 2024-05-02 07:30 | 1 | **Pass** |
| 2024-08-07 | Kvartalsrapport 2024-Q2 | 2024-08-07 07:31 | 1 | **Pass** |
| 2024-11-06 | Kvartalsrapport 2024-Q3 | 2024-11-06 07:30 | 1 | **Pass** |
| 2025-02-05 | Bokslutskommuniké 2024 | 2025-02-05 07:30 | 1 | **Pass** |
| 2025-05-07 | Kvartalsrapport 2025-Q1 | 2025-05-07 07:30 | 1 | **Pass** |
| 2025-08-06 | Kvartalsrapport 2025-Q2 | 2025-08-06 07:30 | 1 | **Pass** |
| 2025-11-05 | Kvartalsrapport 2025-Q3 | 2025-11-05 07:30 | 1 | **Pass** |
| 2026-02-04 | Bokslutskommuniké 2025 | 2026-02-03 17:38 | 1 | **Pass** |
| 2026-05-06 | Kvartalsrapport 2026-Q1 | 2026-05-06 07:31 | 1 | **Pass** |
| 2026-08-05 | Kvartalsrapport 2026-Q2 | 2026-08-04 18:37 | 1 | **Pass** |
| 2026-11-04 18:35 | Kvartalsrapport 2026-Q3 | scheduled row id 217, 2026-11-04 18:35 | scheduled | **Pass** (upcoming) |

The calendar rows are the *planned* dates. Novo released earlier than planned for 2026-02-04 (published 2026-02-03 17:38) and 2026-08-05 (published 2026-08-04 18:37), both after the close, so the reaction belongs to the next day (the "expected date" and the release differ by one day; signal matches them within 7 days).

### Comparison with MFN's financial calendar, 2011-2026 (all rows)

| Calendar date | Label | Our row (Copenhagen) | Result |
|---|---|---|---|
| 2011-02-02 | Bokslutskommuniké 2010 | 2011-02-02 08:29 | OK |
| 2011-04-28 | Kvartalsrapport 2011-Q1 | - | Missing in MFN's feed too (feed starts 2011-02-02 with the year-end report; no Q1/Q2 2011 item) |
| 2011-08-04 | Kvartalsrapport 2011-Q2 | - | Missing in MFN's feed too (feed starts 2011-02-02 with the year-end report; no Q1/Q2 2011 item) |
| 2011-10-27 | Kvartalsrapport 2011-Q3 | 2011-10-27 07:29 | OK |
| 2012-02-02 | Bokslutskommuniké 2011 | 2012-02-02 07:29 | OK |
| 2012-04-27 | Kvartalsrapport 2012-Q1 | 2012-04-27 07:30 | OK |
| 2012-08-09 | Kvartalsrapport 2012-Q2 | 2012-08-09 07:39 | OK |
| 2012-10-31 | Kvartalsrapport 2012-Q3 | - | **Missing**: MFN has the item (2012-10-31T06:33Z), untagged, not classified (NEX-198) |
| 2013-02-04 | Bokslutskommuniké 2012 | - | **Missing**: MFN has the item (2013-01-31T06:29Z), untagged, not classified (NEX-198) |
| 2013-05-01 | Kvartalsrapport 2013-Q1 | 2013-05-01 07:30 | OK |
| 2013-08-08 | Kvartalsrapport 2013-Q2 | - | **Missing**: MFN has the item (2013-08-08T05:30Z), untagged, not classified (NEX-198) |
| 2013-10-31 | Kvartalsrapport 2013-Q3 | 2013-10-31 07:31 | OK |
| 2014-02-03 | Bokslutskommuniké 2013 | - | **Missing**: MFN has the item (2014-01-30T06:31Z), untagged, not classified (NEX-198) |
| 2014-05-01 | Kvartalsrapport 2014-Q1 | 2014-05-01 07:30 | OK |
| 2014-08-07 | Kvartalsrapport 2014-Q2 | 2014-08-07 07:30 | OK |
| 2014-10-30 | Kvartalsrapport 2014-Q3 | - | **Missing**: MFN has the item (2014-10-30T06:31Z), untagged, not classified (NEX-198) |
| 2015-01-30 | Bokslutskommuniké 2014 | - | **Missing**: MFN has the item (2015-01-30T06:30Z), untagged, not classified (NEX-198) |
| 2015-04-30 | Kvartalsrapport 2015-Q1 | 2015-04-30 07:30 | OK |
| 2015-08-06 | Kvartalsrapport 2015-Q2 | 2015-08-06 07:31 | OK |
| 2015-10-29 | Kvartalsrapport 2015-Q3 | - | **Missing**: MFN has the item (2015-10-29T06:30Z), untagged, not classified (NEX-198) |
| 2016-02-03 | Bokslutskommuniké 2015 | - | **Missing**: MFN has the item (2016-02-03T06:30Z), untagged, not classified (NEX-198) |
| 2016-04-29 | Kvartalsrapport 2016-Q1 | 2016-04-29 07:30 | OK |
| 2016-08-05 | Kvartalsrapport 2016-Q2 | - | **Missing**: MFN has the item (2016-08-05T05:30Z), untagged, not classified (NEX-198) |
| 2016-10-28 | Kvartalsrapport 2016-Q3 | - | **Missing**: MFN has the item (2016-10-28T05:30Z), untagged, not classified (NEX-198) |
| 2017-02-02 | Bokslutskommuniké 2016 | - | **Missing**: MFN has the item (2017-02-02T06:30Z), untagged, not classified (NEX-198) |
| 2017-05-03 | Kvartalsrapport 2017-Q1 | - | **Missing**: MFN has the item (2017-05-03T05:30Z), untagged, not classified (NEX-198) |
| 2017-08-09 | Kvartalsrapport 2017-Q2 | - | **Missing**: MFN has the item (2017-08-09T05:31Z), untagged, not classified (NEX-198) |
| 2017-11-01 | Kvartalsrapport 2017-Q3 | - | **Missing**: MFN has the item (2017-11-01T06:30Z), untagged, not classified (NEX-198) |
| 2018-02-01 | Bokslutskommuniké 2017 | - | **Missing**: MFN has the item (2018-02-01T06:30Z), untagged, not classified (NEX-198) |
| 2018-05-02 | Kvartalsrapport 2018-Q1 | - | **Missing**: MFN has the item (2018-05-02T05:31Z), untagged, not classified (NEX-198) |
| 2018-08-08 | Kvartalsrapport 2018-Q2 | - | **Missing**: MFN has the item (2018-08-08T05:30Z), untagged, not classified (NEX-198) |
| 2019-02-01 | Bokslutskommuniké 2018 | - | **Missing**: MFN has the item (2019-02-01T06:30Z), untagged, not classified (NEX-198) |
| 2019-05-03 | Kvartalsrapport 2019-Q1 | - | **Missing**: MFN has the item (2019-05-03T05:30Z), untagged, not classified (NEX-198) |
| 2019-08-21 | Kvartalsrapport 2019-Q2 | 2019-08-09 07:31 | OK (the calendar date is 12 days after the actual release; MFN item and our row are 2019-08-09) |
| 2019-11-01 | Kvartalsrapport 2019-Q3 | - | **Missing**: MFN has the item (2019-11-01T06:31Z), untagged, not classified (NEX-198) |
| 2020-02-05 | Bokslutskommuniké 2019 | 2020-02-05 07:31 | OK |
| 2020-05-06 | Kvartalsrapport 2020-Q1 | 2020-05-06 07:31 | OK |
| 2020-08-06 | Kvartalsrapport 2020-Q2 | 2020-08-06 07:30 | OK |
| 2020-10-30 | Kvartalsrapport 2020-Q3 | 2020-10-30 07:30 | OK |
| 2021-02-03 | Bokslutskommuniké 2020 | 2021-02-03 07:30 | OK |
| 2021-05-05 | Kvartalsrapport 2021-Q1 | 2021-05-05 07:30 | OK |
| 2021-08-05 | Kvartalsrapport 2021-Q2 | 2021-08-04 16:24 | OK |
| 2021-11-03 | Kvartalsrapport 2021-Q3 | 2021-11-03 07:30 | OK |
| 2022-02-02 | Bokslutskommuniké 2021 | 2022-02-02 07:30 | OK |
| 2022-05-04 | Kvartalsrapport 2022-Q1 | 2022-04-29 08:39 | OK |
| 2022-08-04 | Kvartalsrapport 2022-Q2 | 2022-08-03 16:36 | OK |
| 2022-11-02 | Kvartalsrapport 2022-Q3 | 2022-11-02 07:30 | OK |
| 2023-02-01 | Bokslutskommuniké 2022 | 2023-02-01 07:30 | OK |
| 2023-05-04 | Kvartalsrapport 2023-Q1 | 2023-05-04 07:31 | OK |
| 2023-08-10 | Kvartalsrapport 2023-Q2 | 2023-08-10 07:30 | OK |
| 2023-11-02 | Kvartalsrapport 2023-Q3 | 2023-11-02 07:30 | OK |
| 2024-01-31 | Bokslutskommuniké 2023 | 2024-01-31 07:30 | OK |
| 2024-05-02 | Kvartalsrapport 2024-Q1 | 2024-05-02 07:30 | OK |
| 2024-08-07 | Kvartalsrapport 2024-Q2 | 2024-08-07 07:31 | OK |
| 2024-11-06 | Kvartalsrapport 2024-Q3 | 2024-11-06 07:30 | OK |
| 2025-02-05 | Bokslutskommuniké 2024 | 2025-02-05 07:30 | OK |
| 2025-05-07 | Kvartalsrapport 2025-Q1 | 2025-05-07 07:30 | OK |
| 2025-08-06 | Kvartalsrapport 2025-Q2 | 2025-08-06 07:30 | OK |
| 2025-11-05 | Kvartalsrapport 2025-Q3 | 2025-11-05 07:30 | OK |
| 2026-02-04 | Bokslutskommuniké 2025 | 2026-02-03 17:38 | OK |
| 2026-05-06 | Kvartalsrapport 2026-Q1 | 2026-05-06 07:31 | OK |
| 2026-08-05 | Kvartalsrapport 2026-Q2 | 2026-08-04 18:37 | OK |

Totals: 40 published, 20 missing but in MFN (NEX-198), 2 not in MFN at all. A further MFN item, Q3 2018 (2018-11-01 06:30Z), has no calendar row and is not classified either (NEX-198 lists 21). Every published report appears exactly once.

## 2. Upcoming

```
scheduled | quarterly_report | id 217 | expected_date 2026-11-04 | expected_time 18:35:00 | Kvartalsrapport 2026-Q3 | 2026-11-04 18:35 Copenhagen | calendar_fetched_at 2026-10-27 12:11:50+00
```
The row exists with `status = 'scheduled'` and the time MFN estimates (the previous report's evening time). The validation ran on 2026-10-27, **before** 2026-11-04, so the conversion to an `occurred` row has **not** been checked; repeat query R2 after the release.

## 3. Reactions: all 31 `ok` reports against independent closes

Our columns come from `special_events` (`base_close`, `reaction_close`, `move_pct`); the independent columns are Yahoo's closes for the same two dates. The base and reaction dates, and the session, were also derived independently (Yahoo's trading days and `Europe/Copenhagen`): **31 of 31 identical**. Differences are in per cent of the independent close.

| Released (Copenhagen) | Kind | Session | Reaction day | Base day | Our base | Our reaction | Our move | Yahoo base | Yahoo reaction | Yahoo move | Diff base | Diff reaction | Dates |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 2015-04-30 07:30 | interim | pre_open | 2015-04-30 | 2015-04-29 | 186.5 | 189.35 | +1.5282 % | 186.5000 | 189.3500 | +1.5282 % | +0.000 % | -0.000 % | ok |
| 2015-08-06 07:31 | interim | pre_open | 2015-08-06 | 2015-08-05 | 205.35 | 198.25 | -3.4575 % | 205.3500 | 198.2500 | -3.4575 % | -0.000 % | +0.000 % | ok |
| 2016-04-29 07:30 | interim | pre_open | 2016-04-29 | 2016-04-28 | 185.5 | 181.45 | -2.1833 % | 185.5000 | 181.4500 | -2.1833 % | +0.000 % | +0.000 % | ok |
| 2019-08-09 07:31 | interim | pre_open | 2019-08-09 | 2019-08-08 | 167.4 | 167 | -0.2389 % | 167.4000 | 167.0000 | -0.2389 % | +0.000 % | +0.000 % | ok |
| 2020-02-05 07:31 | annual | pre_open | 2020-02-05 | 2020-02-04 | 207.975 | 217.5 | +4.5799 % | 207.9750 | 217.5000 | +4.5799 % | -0.000 % | +0.000 % | ok |
| 2020-05-06 07:31 | interim | pre_open | 2020-05-06 | 2020-05-05 | 215.05 | 216.35 | +0.6045 % | 215.0500 | 216.3500 | +0.6045 % | -0.000 % | -0.000 % | ok |
| 2020-08-06 07:30 | interim | pre_open | 2020-08-06 | 2020-08-05 | 203.9 | 200.2 | -1.8146 % | 203.9000 | 200.2000 | -1.8146 % | +0.000 % | +0.000 % | ok |
| 2020-10-30 07:30 | interim | pre_open | 2020-10-30 | 2020-10-29 | 212.525 | 205.275 | -3.4114 % | 212.5250 | 205.2750 | -3.4114 % | +0.000 % | +0.000 % | ok |
| 2021-02-03 07:30 | annual | pre_open | 2021-02-03 | 2021-02-02 | 219 | 225 | +2.7397 % | 219.0000 | 225.0000 | +2.7397 % | +0.000 % | +0.000 % | ok |
| 2021-05-05 07:30 | interim | pre_open | 2021-05-05 | 2021-05-04 | 228.825 | 229.625 | +0.3496 % | 228.8250 | 229.6250 | +0.3496 % | +0.000 % | +0.000 % | ok |
| 2021-08-04 16:24 | interim | intraday | 2021-08-04 | 2021-08-03 | 291.55 | 305.5 | +4.7848 % | 291.5500 | 305.5000 | +4.7848 % | +0.000 % | +0.000 % | ok |
| 2021-11-03 07:30 | interim | pre_open | 2021-11-03 | 2021-11-02 | 357 | 365.8 | +2.4650 % | 357.0000 | 365.8000 | +2.4650 % | +0.000 % | +0.000 % | ok |
| 2022-02-02 07:30 | annual | pre_open | 2022-02-02 | 2022-02-01 | 335.6 | 350.25 | +4.3653 % | 335.6000 | 350.2500 | +4.3653 % | -0.000 % | +0.000 % | ok |
| 2022-04-29 08:39 | interim | pre_open | 2022-04-29 | 2022-04-28 | 385.4 | 406.15 | +5.3840 % | 385.4000 | 406.1500 | +5.3840 % | +0.000 % | +0.000 % | ok |
| 2022-08-03 16:36 | interim | intraday | 2022-08-03 | 2022-08-02 | 425.6 | 385.6 | -9.3985 % | 425.6000 | 385.6000 | -9.3985 % | -0.000 % | -0.000 % | ok |
| 2022-11-02 07:30 | interim | pre_open | 2022-11-02 | 2022-11-01 | 400.5 | 430.3 | +7.4407 % | 400.5000 | 430.3000 | +7.4407 % | +0.000 % | +0.000 % | ok |
| 2023-02-01 07:30 | annual | pre_open | 2023-02-01 | 2023-01-31 | 471.5 | 471.2 | -0.0636 % | 471.5000 | 471.2000 | -0.0636 % | +0.000 % | -0.000 % | ok |
| 2023-05-04 07:31 | interim | pre_open | 2023-05-04 | 2023-05-03 | 561.6 | 547 | -2.5997 % | 561.6000 | 547.0000 | -2.5997 % | +0.000 % | +0.000 % | ok |
| 2023-08-10 07:30 | interim | pre_open | 2023-08-10 | 2023-08-09 | 631.7 | 624 | -1.2189 % | 631.7000 | 624.0000 | -1.2189 % | -0.000 % | +0.000 % | ok |
| 2023-11-02 07:30 | interim | pre_open | 2023-11-02 | 2023-11-01 | 687.7 | 709.5 | +3.1700 % | 687.7000 | 709.5000 | +3.1700 % | -0.000 % | +0.000 % | ok |
| 2024-01-31 07:30 | annual | pre_open | 2024-01-31 | 2024-01-30 | 748.2 | 774.8 | +3.5552 % | 748.2000 | 774.8000 | +3.5552 % | -0.000 % | +0.000 % | ok |
| 2024-05-02 07:30 | interim | pre_open | 2024-05-02 | 2024-05-01 | 897.7 | 873.1 | -2.7403 % | 897.7000 | 873.1000 | -2.7403 % | -0.000 % | +0.000 % | ok |
| 2024-08-07 07:31 | interim | pre_open | 2024-08-07 | 2024-08-06 | 888 | 828.4 | -6.7117 % | 888.0000 | 828.4000 | -6.7117 % | +0.000 % | -0.000 % | ok |
| 2024-11-06 07:30 | interim | pre_open | 2024-11-06 | 2024-11-05 | 745.4 | 749.5 | +0.5500 % | 745.4000 | 749.5000 | +0.5500 % | -0.000 % | +0.000 % | ok |
| 2025-02-05 07:30 | annual | pre_open | 2025-02-05 | 2025-02-04 | 592.6 | 619.5 | +4.5393 % | 592.6000 | 619.5000 | +4.5393 % | +0.000 % | +0.000 % | ok |
| 2025-05-07 07:30 | interim | pre_open | 2025-05-07 | 2025-05-06 | 441.05 | 446.8 | +1.3037 % | 441.0500 | 446.8000 | +1.3037 % | +0.000 % | +0.000 % | ok |
| 2025-08-06 07:30 | interim | pre_open | 2025-08-06 | 2025-08-05 | 305.9 | 289.5 | -5.3612 % | 305.9000 | 289.5000 | -5.3612 % | +0.000 % | +0.000 % | ok |
| 2025-11-05 07:30 | interim | pre_open | 2025-11-05 | 2025-11-04 | 315.2 | 301.05 | -4.4892 % | 315.2000 | 301.0500 | -4.4892 % | -0.000 % | +0.000 % | ok |
| 2026-02-03 17:38 | annual | after_close | 2026-02-04 | 2026-02-03 | 367.8 | 304.65 | -17.1697 % | 367.8000 | 304.6500 | -17.1697 % | +0.000 % | +0.000 % | ok |
| 2026-05-06 07:31 | interim | pre_open | 2026-05-06 | 2026-05-05 | 285.9 | 293 | +2.4834 % | 285.9000 | 293.0000 | +2.4834 % | +0.000 % | +0.000 % | ok |
| 2026-08-04 18:37 | interim | after_close | 2026-08-05 | 2026-08-04 | 307.6 | 294.3 | -4.3238 % | 307.6000 | 294.3000 | -4.3238 % | -0.000 % | +0.000 % | ok |

Closes agree to the stored precision (Saxo has 6 decimals, Yahoo's are float rounded; the largest difference is 0.000 %), so the 0.5 % tolerance is met by every row. Closing-auction versus last-trade differences did not show up on these dates.

Coverage of the requirement: **after-close** releases: 2026-02-03 17:38 (annual, -17.17 %) and 2026-08-04 18:37 (Q2 2026, -4.32 %), both with the reaction on the next day; **annual** reports: 2020-02-05, 2021-02-03, 2022-02-02, 2023-02-01, 2024-01-31, 2025-02-05, 2026-02-03; **before September 2023**: 19 rows from 2015-04-30 to 2023-08-10; **intraday**: 2021-08-04 16:24 and 2022-08-03 16:36 (before the 17:00 close, so the move is the same day's).

What the real data does **not** exercise: no Novo report was released on a Copenhagen-only closure (Friday after Ascension, 5 June, 24 or 31 December), none between 17:00 and 17:30 (where Stockholm's hours would differ), and no base and reaction day have a holiday between them. Those cases are covered by the unit and sqlx tests of NEX-191 only.

## 4. The 2023-09-13 split (1:2)

| Date | Saxo open | high | low | close | volume |
|---|---|---|---|---|---|
| 2023-09-08 | 682.8 | 683.4 | 670.9 | 677.6 | 2,790,278 |
| 2023-09-11 | 679.4 | 689.6 | 677.3 | 683.9 | 2,768,046 |
| 2023-09-12 | 688.7 | 690.3 | 673.2 | 681.5 | 2,703,932 |
| **2023-09-13** | 681.5 | 684.6 | 661.0 | **673.7** | 3,073,707 |
| 2023-09-14 | 675.0 | 682.9 | 674.4 | 682.9 | 2,517,001 |
| 2023-09-15 | 679.9 | 685.8 | 666.5 | 666.5 | 7,148,390 |

- **Prices are split-adjusted.** The close moves from 681.50 to 673.70 (-1.14 %) over the split date. An unadjusted series would halve (about -50 %). Yahoo's adjusted series gives the same numbers, and Saxo matches it within 0.001 % on 2,915 of the 2,955 days compared from 2015-01-02 on, including every day before the split.
- **Volumes are not adjusted** (median 2.2 M shares a day from June to 12 September 2023, 2.55 M after; adjusting would have doubled the earlier ones). Irrelevant for the move, but `reaction_volume` is not comparable across the split.
- **Effect on reactions:** none. No report has its base day before 2023-09-13 and its reaction day on or after it (the closest are 2023-08-10 and 2023-11-02), and `needs_review` is set for **0** Novo rows (S3). The largest move in the data is 2026-02-03's -17.17 %.
- This differs from GomSpace, where Saxo's closes were **not** adjusted for a share issue (NEX-165, NEX-185): Saxo adjusts splits but, at least in that case, not share issues. Not generalised here.
- Side note: MFN's calendar lists "Split NOVO B 1:2" with the date 1997-01-02, not 2023-09-13; the calendar row is not used.

### Where Saxo and Yahoo differ

Of 2,955 common days, 11 differ by more than 0.5 % (none of them a report's base or reaction day) and 40 by more than 0.01 %. All are in 2015-11/12 and 2018-01/02:

| Date | Saxo close | Yahoo close | Difference |
|---|---|---|---|
| 2015-11-16 | 184.05 | 185.35 | -0.70 % |
| 2015-11-30 | 194.50 | 193.00 | +0.78 % |
| 2015-12-07 | 197.95 | 194.55 | +1.75 % |
| 2015-12-11 | 189.05 | 194.25 | -2.68 % |
| 2018-01-03 | 168.15 | 169.00 | -0.50 % |
| 2018-01-11 | 168.00 | 169.25 | -0.74 % |
| 2018-01-19 | 175.75 | 174.75 | +0.57 % |
| 2018-01-26 | 174.75 | 173.80 | +0.55 % |
| 2018-01-29 | 171.75 | 172.78 | -0.59 % |
| 2018-01-30 | 169.50 | 170.63 | -0.66 % |
| 2018-02-07 | 152.90 | 152.03 | +0.58 % |

The cause was not investigated (last trade versus closing auction, or a bad tick in one source). The two sources have the same set of trading days (2,956 each from 2015-01-02), and Yahoo has no close for 2026-03-23.

## 5. SQL (production, 2026-10-27)

```sql
-- R1: the reports of section 1 (Copenhagen time)
SELECT id, event_type, to_char(occurred_at AT TIME ZONE 'Europe/Copenhagen','YYYY-MM-DD HH24:MI') AS local_cph,
       lang, report_kind, fiscal_period, release_session, reaction_date, base_date,
       base_close, reaction_close, move_pct, data_status, needs_review, superseded_by, description
FROM special_events
WHERE ticker='NOVO-B' AND exchange_mic='XCSE' AND source='mfn' AND status='occurred'
ORDER BY occurred_at;

-- R2: the scheduled row (section 2)
SELECT id, status, event_type, expected_date, expected_time, calendar_label, calendar_fetched_at
FROM special_events WHERE ticker='NOVO-B' AND exchange_mic='XCSE' AND status='scheduled';
-- 217 | scheduled | quarterly_report | 2026-11-04 | 18:35:00 | Kvartalsrapport 2026-Q3 | 2026-10-27 12:11:50+00

-- S1: reaction before the release's local date. Result: 0
SELECT count(*) FROM special_events WHERE ticker='NOVO-B' AND exchange_mic='XCSE' AND source='mfn' AND status='occurred'
  AND event_type IN ('quarterly_report','annual_report')
  AND reaction_date < (occurred_at AT TIME ZONE 'Europe/Copenhagen')::date;

-- S2a: corrections carrying reaction values. Result: 0 (of 8 corrections)
SELECT count(*) FROM special_events WHERE ticker='NOVO-B' AND exchange_mic='XCSE' AND is_correction
  AND (data_status IS NOT NULL OR release_session IS NOT NULL OR reaction_date IS NOT NULL OR move_pct IS NOT NULL);

-- S2c: non-report titles among the reports. Result: 0
SELECT count(*) FROM special_events WHERE ticker='NOVO-B' AND exchange_mic='XCSE'
  AND event_type IN ('quarterly_report','annual_report')
  AND description ~* '(repurchase|aktietilbagek|managerial|trading in novo|persons discharging|voting rights|major shareholder)';

-- S3: needs_review. Result: 0
SELECT count(*) FROM special_events WHERE ticker='NOVO-B' AND exchange_mic='XCSE' AND needs_review;

-- S4: counts per data_status and per release_session (quarterly and annual reports)
SELECT data_status, count(*) FROM special_events WHERE ticker='NOVO-B' AND exchange_mic='XCSE' AND source='mfn'
  AND status='occurred' AND event_type IN ('quarterly_report','annual_report') GROUP BY 1;
SELECT release_session, count(*) FROM special_events WHERE ticker='NOVO-B' AND exchange_mic='XCSE' AND source='mfn'
  AND status='occurred' AND event_type IN ('quarterly_report','annual_report') GROUP BY 1;
```

| Check | Result |
|---|---|
| S1 look-ahead | **0** |
| S2a corrections with reaction values | **0** (8 corrections in total) |
| S2c non-report titles among the reports | **0** |
| S3 `needs_review` | **0** |
| S4 `data_status` | `ok` 31, `calendar_unknown` 9 (all 9 released 2011-2014, before the DK holidays and Saxo bars start) |
| S4 `release_session` | `pre_open` 27, `intraday` 2, `after_close` 2, NULL 9 (the same 9) |
| S5 largest moves | 2026-02-03 -17.1697, 2022-08-03 -9.3985, 2022-11-02 +7.4407, 2024-08-07 -6.7117, 2022-04-29 +5.3840 |
| Never computed (`reaction_computed_at IS NULL`) | 0 |
| Setup | `exchanges`: XCSE and FNDK = DK, Europe/Copenhagen, 09:00-17:00, no half day; `trading_holidays` DK 2015-01-01 to 2027-12-31 (140 rows, 11 in 2026); `companies`: `NOVO-B`/`XCSE`, DKK, ISIN DK0062498333, slug `novo-nordisk`; `daily_bars`: 2,956 rows 2015-01-02 to 2026-10-27, `source = saxo`, DKK |

## Dashboard

Grafana needs a login (`/api/search` answers 401), so the live dashboard was not opened and there is **no screenshot**. Instead the dashboard's own SQL (`infra/charts/nexus/grafana/templates/dashboard-nordic-reports.yaml`, with the `company` variable set to `NOVO-B|XCSE`) was run against production:

- **Dropdown query:** returns `Novo Nordisk A/S (NOVO B) (NOVO-B)` with value `NOVO-B|XCSE` (next to FirstFarms, GomSpace, Torm and Vestas). The label is doubled because the registry name already ends in "(NOVO B)" and the query only drops the ticker suffix when the name ends in "(NOVO-B)".
- **Upcoming reports:** one row, `2026-11-04 | 18:35 | Kvartalsrapport 2026-Q3 | interim | 2026-Q3`.
- **Past reports:** newest first: 2026-08-04 18:37 after close, reaction 2026-08-05, 307.6 → 294.3, -4.32 %; 2026-05-06 07:31 pre-open, +2.48 %; 2026-02-03 17:38 after close, 2026-02-04, 367.8 → 304.65, -17.17 %; 2025-11-05 07:30 pre-open, -4.49 %; and so on, with the headline and the MFN link.

Swedish-specific text left in the dashboards (follow-up NEX-200): the "Released (Stockholm)" column and its `Europe/Stockholm` SQL; the session panel description "Stockholm session (open 09:00, close 17:30, 13:00 on half days)", wrong for Copenhagen (17:00, no half days); the Upcoming panel's "from today (Stockholm)" and its `Europe/Stockholm` date; `dashboard-special-events.yaml` line 56. Copenhagen has the same offset as Stockholm, so the times shown are right; only the labels and the hours text are wrong.

## Findings

| | Finding | Follow-up |
|---|---|---|
| F1 | 21 Novo results items from 2012-10 to 2019-11 are not classified as reports: MFN gives them no report tag and their headlines ("increased operating profit by 31% in the first half of 2012", "... in 2016") have no report wording. No report row and no reaction for 2017-2018 and most of 2012-2016 | **NEX-198** |
| F2 | Danish/English twins published 23-30 s apart (5 releases, 2011-2015) are stored as a report plus a `report_correction`; the dashboard then shows the Danish headline for some | **NEX-199** |
| F3 | Dashboard: doubled dropdown label for Novo, Stockholm-only wording | **NEX-200** |
| F4 | "Correction: Resolutions from the Annual General Meeting" (2021-03-25) is stored as a `report_correction` | already NEX-184 (comment added) |
| F5 | Saxo adjusts for splits (this document) but did not for GomSpace's share issue | comment added to NEX-185 |
| F6 | 9 reports from 2011-2014 have `calendar_unknown`: the DK holidays are seeded from 2015 and Saxo's backfill starts 2015-01-01. By design; extending both back would add reactions for 2011-2014 | none (decision for the user) |
| F7 | Novo released 2026-02-03 and 2026-08-04 after the close and a day before the calendar date; both are classified `after_close` with the reaction on the next day, as intended | none |

## Pass criteria

| Criterion | Result |
|---|---|
| Every published report appears exactly once; no repurchase or PDMR announcement | **Pass** (S2c 0; 2011-2026 table: no row twice) |
| Every report in the calendar is published | **Pass for 2020-2026 (27 of 27)**; **fails before 2020** (21 missing, NEX-198) |
| Q3 2026 scheduled row | **Pass**; conversion to `occurred` not checked (before 2026-11-04) |
| At least 8 reports with an after-close release, an annual report and one before September 2023 | **31 reports** (2 after close, 7 annual, 19 before 2023-09) |
| Closes within 0.5 % of an independent source | **Pass**, 31 of 31 identical |
| Reaction date follows the session rule with Copenhagen hours and holidays | **Pass**, 31 of 31 (independent calendar, `Europe/Copenhagen`, 17:00 close); holiday-specific cases only by unit tests |
| Split adjusted or not, effect on reactions | **Adjusted**; no effect, `needs_review` 0 |
| Sanity SQL | **Pass** |
| Mismatches become Linear issues, none patched here | NEX-198, NEX-199, NEX-200 |
| Dashboard | **Pass by query**, no screenshot (login required) |

## Reproducing

The comparison used throwaway read-only tooling (a Rust test running the SQL above in a read-only transaction against `backtest`, and Python for the Yahoo and MFN side), not part of the repository. The SQL in section 5 is enough to redo the signal side; the independent side is Yahoo's chart API (Sources) and MFN's public feed and calendar page.
