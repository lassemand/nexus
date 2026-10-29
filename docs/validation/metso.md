# Metso (METSO, XHEL): reports, upcoming dates, reactions and insider transactions checked against independent sources

NEX-208. A wrong number looks exactly like a right one, so the end result for Metso is checked against sources that are not our own pipeline. It mirrors the Novo validation ([`nordic-report-reactions-novo.md`](nordic-report-reactions-novo.md)) and the GomSpace one ([`nordic-report-reactions.md`](nordic-report-reactions.md)).

## Status: the deployed system could not be checked yet, the independent half is done

| | What | Result |
|---|---|---|
| **Production today (2026-10-29)** | Is Metso in the system? | **No data yet.** `backtest.companies` has `METSO / XHEL / EUR` but **no MFN slug and no ISIN**; chronicle's `companies` has no Metso; `special_events`, `daily_bars` and `insider_filings` have **0** Metso rows; `exchanges` has **no** `XHEL` row and `trading_holidays` **no** `FI` rows (signal's last applied migration is `20261027000001`, before the Finnish `20261028000000_exchanges_add_finland`). See [What is missing](#what-is-missing-before-the-system-can-be-checked) |
| Reports 2023-2026 | The rows the pipeline would produce vs Nasdaq's own release feed | **16 of 16 real reports match Nasdaq to the minute, each exactly once** (fi/en pairs merged). One extra row, an orphan correction of a bond issue ([NEX-213](https://linear.app/nexusboard/issue/NEX-213)) |
| Identity | Neles / Outotec | **0 of 124** report events mention Neles. All 59 rows before 2020-07-01 are Outotec's |
| Upcoming | MFN's calendar vs Metso's own announcement of 2026-10-12 | **4 of 4 dates match**; the 2026 full-year date is already announced: **2027-02-19** |
| Reactions | Expected session, reaction day, base day and move for the 16 reports, from independent closes | **Computed (all 16 `pre_open`, same day).** The comparison with signal's rows is **blocked**: no rows exist yet |
| History | Closes across the 2020-06-30 / 2020-07-01 merger | **No jump**: EUR 4.912 then 4.946 (+0.69 %) |
| Insiders | All 40 managers' transaction releases of 2023-2026 vs Nasdaq's release pages | **40 of 40 map to one row each; 39 match volume, date and price exactly**, 1 differs because the issuer's English copy has a wrong year ([NEX-214](https://linear.app/nexusboard/issue/NEX-214)). 2026-03-20 and 2026-04-24 as expected; **0 BUY at price 0** |
| Dashboard | Grafana Nordic Reports / Event Log | **Not checked**: nothing to show yet |

**Verdict: 9 of 9 checks that do not need the deployed system pass; 0 of 8 checks that do could be run** (a ninth does not apply to Metso) (listed under [Pass criteria](#pass-criteria)). Four findings are open as bugs: [NEX-212](https://linear.app/nexusboard/issue/NEX-212), [NEX-213](https://linear.app/nexusboard/issue/NEX-213), [NEX-214](https://linear.app/nexusboard/issue/NEX-214), [NEX-215](https://linear.app/nexusboard/issue/NEX-215).

"The pipeline would produce" means: the real code run on the real data, outside the cluster. For reports, `chronicle_lib::nordic::classify` and `events::release_event` over Metso's MFN feed, then signal's twin rule (same company, instant and type, English preferred) applied by hand. For insiders, `chronicle_lib::nordic::insiders::build` (the code `nordic-reports` calls) over the same feed. Nothing was sent to Kafka or stored. These are the rows to expect, not rows that exist.

## What is missing before the system can be checked

1. Deploy the images with the Finnish work (signal's migration `20261028000000_exchanges_add_finland`, chronicle's `NORDIC_MICS`, `saxo-stream` with `XHEL`).
2. Seed the Finnish holidays: `nexus calendar sync --country FI --from-year 2015 --to-year 2027`.
3. Register Metso with its mapping: `nexus register METSO.HE`. Today's row in `backtest.companies` has no `mfn_slug` or ISIN, consistent with a registration made before the registry change; running it again should fill them in (expect `MFN metso-2 (Metso)` and `ISIN FI0009014575`). Check in both databases: `SELECT ticker, exchange_mic, currency, mfn_slug, isin FROM companies WHERE ticker = 'METSO'`.
4. Run `nordic-reports` (22:00, or a manual Job) with `--backfill`, `nordic-calendar`, wait for `saxo-stream` to backfill the bars (EUR, from 2015-01-01), and run the insider backfill Job:
   `nordic-reports --insiders-only --ticker METSO --mic XHEL --from 2016-07-03` (the exact `kubectl` command is in `docs/guides/nordic-companies.md`).
5. The Saxo ExchangeIds for Helsinki (`HSE`, `HSE_FN-FI`) are unverified; if the first sweep fails to resolve Metso, set `SAXO_EXCHANGE_IDS`.

Then run the queries under [SQL for the deployed system](#sql-for-the-deployed-system) and compare with the tables below.

## Sources and method

- **Our data:** production signal database `backtest` and chronicle's database, read-only (`SET TRANSACTION READ ONLY`, SELECT only), 2026-10-29.
- **Independent release feed:** Nasdaq's news service `https://api.news.eu.nasdaq.com/news/query.action?globalGroup=companyNews&company=Metso%20Corporation` (1,482 items, all languages, with Nasdaq's own disclosure category such as "Interim report (Q1 and Q3)", "Financial Statement Release", "Managers' Transactions"), fetched 2026-10-29. It is the exchange's distribution of the issuer's releases, not MFN.
- **Independent release text:** the same releases as Nasdaq shows them (`view.news.eu.nasdaq.com`), for the managers' transactions.
- **Independent announcement of dates:** Metso's own release of 2026-10-12, "Metso's financial reporting and Annual General Meeting in 2027" (Nasdaq category "Financial Calendar"). Metso's investor site (`metso.com/corporate/investors/calendar/`) builds its calendar in the browser, so it could not be read.
- **Independent prices:** Yahoo Finance `METSO.HE` (`exchangeName HEL`, EUR, "Metso Oyj"), `https://query1.finance.yahoo.com/v8/finance/chart/METSO.HE`, 2,973 daily bars 2015-01-02 to 2026-10-29, `close` (not the adjusted close), with `events=div`. Nasdaq Helsinki's historical prices were not reachable programmatically and Stooq answers bots with a JavaScript proof-of-work page, which I did not circumvent. **One price source only, so no row has a second price source.**
- **Trading days:** Yahoo's own bars. Over 2015-2026 there is no weekday without a Yahoo bar that is not a closure of the Finnish calendar from the exchange-model task (Epiphany, Good Friday, Easter Monday, 1 May, Ascension, Midsummer Eve, Independence Day, 24, 25, 26 and 31 December), and no Yahoo bar on a closure.
- **Rules applied by hand for the reactions:** session from the release instant in `Europe/Helsinki` (before 10:00 pre-open, 10:00 up to 18:30 intraday, then after the close); reaction day r the same day for pre-open and intraday, otherwise the next trading day; base day b the trading day before r; `move_pct = (close_r - close_b) / close_b * 100`.

## 1. Reports, 2023-01-01 to now

The 17 rows the pipeline would store (`source = 'mfn'`, `status = 'occurred'`), Finnish and English copies merged to one row each, the English one kept. The column "Nasdaq category" is Nasdaq's own classification of the release published at **exactly that instant**.

| # | Released (UTC) | Helsinki | Kind | Period | Lang | Nasdaq category (independent) | Title |
|---|---|---|---|---|---|---|---|
| 1 | 2023-02-17 07:00 | 2023-02-17 09:00 | annual | 2022 | en | Financial Statement Release | Metso Outotec's Financial Statements Review January 1 - December 31, 2022 |
| 2 | 2023-05-03 06:00 | 2023-05-03 09:00 | quarterly | 2023-Q1 | en | Interim report (Q1 and Q3) | Metso Outotec's Interim Report January - March 2023 |
| 3 | 2023-07-20 06:00 | 2023-07-20 09:00 | quarterly | 2023-Q2 | en | Half Year financial report | Metso's Half-Year Report for January-June 2023 |
| 4 | 2023-10-27 06:00 | 2023-10-27 09:00 | quarterly | 2023-Q3 | en | Interim report (Q1 and Q3) | Metso's Interim Report for January-September 2023 |
| 5 | 2024-02-16 07:00 | 2024-02-16 09:00 | annual | 2023 | en | Financial Statement Release | Metso's Financial Statements Review January-December 31, 2023 |
| 6 | 2024-04-25 06:00 | 2024-04-25 09:00 | quarterly | 2024-Q1 | en | Interim report (Q1 and Q3) | Metso's Interim Report for January-March 2024 |
| 7 | 2024-07-24 06:00 | 2024-07-24 09:00 | quarterly | 2024-Q2 | en | Half Year financial report | Metso's Half-Year Report for January-June 2024 |
| 8 | 2024-10-24 06:00 | 2024-10-24 09:00 | quarterly | 2024-Q3 | en | Interim report (Q1 and Q3) | Metso's Interim Report for January-September 2024 |
| 9 | 2025-02-13 07:00 | 2025-02-13 09:00 | annual | 2024 | en | Financial Statement Release | Metso's Financial Statements Review January 1 - December 31, 2024 |
| 10 | 2025-04-24 06:00 | 2025-04-24 09:00 | quarterly | 2025-Q1 | en | Interim report (Q1 and Q3) | Metso's Interim Report January 1 - March 31, 2025 |
| 11 | 2025-05-21 18:42 | 2025-05-21 21:42 | report_correction | - | en | **none** | Correction: Metso issues a EUR 300 million bond |
| 12 | 2025-07-23 06:00 | 2025-07-23 09:00 | quarterly | 2025-Q2 | en | Half Year financial report | Metso's Half-Year Report January 1 - June 30, 2025 |
| 13 | 2025-10-23 06:00 | 2025-10-23 09:00 | quarterly | 2025-Q3 | en | Interim report (Q1 and Q3) | Metso's Interim Report January 1 - September 30, 2025 |
| 14 | 2026-02-12 07:00 | 2026-02-12 09:00 | annual | 2025 | en | Financial Statement Release | Metso's Financial Statements Review January 1 - December 31, 2025 |
| 15 | 2026-04-22 06:00 | 2026-04-22 09:00 | quarterly | 2026-Q1 | en | Interim report (Q1 and Q3) | Metso's Interim Report January 1 - March 31, 2026 |
| 16 | 2026-07-24 06:00 | 2026-07-24 09:00 | quarterly | 2026-Q2 | en | Half Year financial report | Metso's Half-Year Report January 1 - June 30, 2026 |
| 17 | 2026-10-22 06:00 | 2026-10-22 09:00 | quarterly | 2026-Q3 | en | Interim report (Q1 and Q3) | Metso's Interim Report January 1 - September 30, 2026 |

- **16 real reports, 16 matches.** Nasdaq lists 4 full-year (Financial Statement Release), 4 half-year, and 8 Q1/Q3 releases for 2023-2026, in Finnish and English: 16 events, 32 items. They are the 16 rows above, at the same minute. No quarter is missing and none appears twice.
- The annual report *documents* (Nasdaq "Annual Financial Report": 2023-03-22, 2024-03-20, 2025-03-26, 2026-03-04) are correctly not rows.
- **Row 11 is not a report.** "Correction: Metso issues a EUR 300 million bond" is a correction of a bond release. The classifier stores any `Correction:` title as a correction; it has no report within 7 days to attach to, so it would show as an orphan row in the Event Log ([NEX-213](https://linear.app/nexusboard/issue/NEX-213)).
- **No Neles item.** Over the whole `metso-2` feed (1,920 items, 2012-04 to 2026-10) the pipeline produces 124 report events, **none with "Neles" in the title**. The 59 merged rows before 2020-07-01 all say "Outotec". The feed contains Metso's releases under the `outotec`, `metso-outotec` and `metso-2` entities, which only the ISIN FI0009014575 ties together; the old Metso (FI0009007835) and Neles (FI4000440664) are other entities and never appear.
- **Duplicates before 2020.** Up to 2019 the Finnish and English copy are published seconds apart (e.g. `2012-07-27T06:03:28Z` and `06:03:41Z`), so the exact-instant twin rule would keep both: 24 such pairs (14 of them from 2015, where the price history starts). Not in the 2023+ window; [NEX-212](https://linear.app/nexusboard/issue/NEX-212).

## 2. Upcoming dates

Metso's release of 2026-10-12 (Nasdaq, "Financial Calendar"): *Financial Statements Review for 2026 on February 19, 2027; Annual Report for 2026 during the week commencing March 1, 2027; Interim Report January-March 2027 on April 19, 2027; Half-Year Financial Report on July 22, 2027; Interim Report January-September 2027 on October 25, 2027; the AGM on April 19, 2027.*

MFN's calendar page `https://mfn.se/a/metso-2` (fetched 2026-10-29), upcoming rows:

| MFN date | MFN time (Stockholm) | MFN label | Metso's announcement | Stored as `occurred_at` (UTC) | In Helsinki |
|---|---|---|---|---|---|
| 2027-02-19 | 08:00 | Bokslutskommuniké 2026 | Financial Statements Review for 2026, 2027-02-19 | 2027-02-19 07:00Z (CET) | 09:00 |
| 2027-04-19 | 08:00 | Kvartalsrapport 2027-Q1 | Interim Report Jan-Mar, 2027-04-19 | 2027-04-19 06:00Z (CEST) | 09:00 |
| 2027-04-19 | N/A | Årsstämma | AGM, 2027-04-19 | (not a report, ignored) | |
| 2027-07-22 | 08:00 | Kvartalsrapport 2027-Q2 | Half-Year Report, 2027-07-22 | 2027-07-22 06:00Z | 09:00 |
| 2027-10-25 | 08:00 | Kvartalsrapport 2027-Q3 | Interim Report Jan-Sep, 2027-10-25 | 2027-10-25 06:00Z (summer time ends 2027-10-31) | 09:00 |

**4 of 4 dates match.** MFN's 08:00 is Stockholm time, which is Metso's usual 09:00 Helsinki (the releases above all go out at 09:00 EET/EEST). The answer to "has a 2026 full-year date been announced": yes, 2027-02-19, already on MFN's page. (The Annual Report for 2026 "during the week commencing March 1" is a document and not in the calendar table.) MFN also lists the ex-dividend day `2026-10-22 X-dag halvårsutdelning METSO 0.2`, ignored as a non-report.

## 3. Reactions

All 16 reports were released at 09:00 Helsinki, before the 10:00 open, so every reaction is the same day's close against the previous trading day's close. Closes are Yahoo's (EUR, as given). This is what signal should store; the system's own rows (`release_session`, `reaction_date`, `base_date`, `base_close`, `reaction_close`, `move_pct`) do not exist yet.

| # | Released (Helsinki) | Kind / period | Session | Reaction day r | Base day b | close_b (EUR) | close_r (EUR) | move_pct | Ex-dividend on r (Yahoo) |
|---|---|---|---|---|---|---|---|---|---|
| 1 | 2023-02-17 09:00 | annual 2022 | pre_open | 2023-02-17 | 2023-02-16 | 10.580 | 10.240 | -3.21 % | - |
| 2 | 2023-05-03 09:00 | quarterly 2023-Q1 | pre_open | 2023-05-03 | 2023-05-02 | 9.896 | 10.580 | +6.91 % | - |
| 3 | 2023-07-20 09:00 | quarterly 2023-Q2 | pre_open | 2023-07-20 | 2023-07-19 | 11.290 | 10.555 | -6.51 % | - |
| 4 | 2023-10-27 09:00 | quarterly 2023-Q3 | pre_open | 2023-10-27 | 2023-10-26 | 8.864 | 8.120 | -8.39 % | 0.15 EUR |
| 5 | 2024-02-16 09:00 | annual 2023 | pre_open | 2024-02-16 | 2024-02-15 | 9.540 | 10.395 | +8.96 % | - |
| 6 | 2024-04-25 09:00 | quarterly 2024-Q1 | pre_open | 2024-04-25 | 2024-04-24 | 10.895 | 10.575 | -2.94 % | - |
| 7 | 2024-07-24 09:00 | quarterly 2024-Q2 | pre_open | 2024-07-24 | 2024-07-23 | 9.638 | 9.142 | -5.15 % | - |
| 8 | 2024-10-24 09:00 | quarterly 2024-Q3 | pre_open | 2024-10-24 | 2024-10-23 | 9.414 | 8.826 | -6.25 % | 0.18 EUR |
| 9 | 2025-02-13 09:00 | annual 2024 | pre_open | 2025-02-13 | 2025-02-12 | 9.770 | 10.600 | +8.50 % | - |
| 10 | 2025-04-24 09:00 | quarterly 2025-Q1 | pre_open | 2025-04-24 | 2025-04-23 | 8.832 | 9.454 | +7.04 % | - |
| 11 | 2025-07-23 09:00 | quarterly 2025-Q2 | pre_open | 2025-07-23 | 2025-07-22 | 11.645 | 11.290 | -3.05 % | - |
| 12 | 2025-10-23 09:00 | quarterly 2025-Q3 | pre_open | 2025-10-23 | 2025-10-22 | 12.145 | 13.700 | +12.80 % | 0.19 EUR |
| 13 | 2026-02-12 09:00 | annual 2025 | pre_open | 2026-02-12 | 2026-02-11 | 17.245 | 16.525 | -4.18 % | - |
| 14 | 2026-04-22 09:00 | quarterly 2026-Q1 | pre_open | 2026-04-22 | 2026-04-21 | 15.730 | 14.940 | -5.02 % | - |
| 15 | 2026-07-24 09:00 | quarterly 2026-Q2 | pre_open | 2026-07-24 | 2026-07-23 | 15.600 | 16.860 | +8.08 % | - |
| 16 | 2026-10-22 09:00 | quarterly 2026-Q3 | pre_open | 2026-10-22 | 2026-10-21 | 16.310 | 16.070 | -1.47 % | 0.20 EUR |

- **Range:** -8.39 % to +12.80 %. Nothing is near the 40 % `needs_review` threshold, so no Metso row should be flagged.
- **Ex-dividend days.** The four Q3 reports (rows 4, 8, 12, 16) fall on the day the half-year dividend goes ex (Yahoo: 0.15, 0.18, 0.19, 0.20 EUR). The unadjusted close drops by the dividend on that day, so those moves include about 1.2-1.9 percentage points of dividend (row 16: 0.20 / 16.31 = 1.2 of the -1.47 %). Whether Saxo's bars are adjusted is unverified. [NEX-215](https://linear.app/nexusboard/issue/NEX-215).
- **A holiday next to the base or reaction day.** None of Metso's 16 reports has one: a pre-open release has its base on the previous trading day, and none of those days follows a Finnish holiday. Holiday handling for Helsinki is covered by the unit and database tests of the reactions task (`helsinki_sessions_and_reaction_days`: Epiphany, Independence Day, New Year, Midsummer Eve, Good Friday/Easter Monday, and the same-instant `XSTO` comparison).
- **Comparison with signal / Saxo:** blocked. Tolerance to apply when it exists: closes within EUR 0.01, or explained (adjustment); compare dates and signs exactly.

### History continuity across the merger date

Yahoo `METSO.HE`, `close`, EUR:

| Date | Close |
|---|---|
| 2020-06-26 | 5.075 |
| 2020-06-29 | 5.185 |
| **2020-06-30** | **4.912** |
| **2020-07-01** | **4.946** |
| 2020-07-02 | 4.704 |

The step at the switch is +0.69 %, far below 15 %. The 2019 closes are Outotec's (EUR 3.88 on 2019-03-15, 4.08 on 2019-06-14, 5.80 on 2019-11-15), not the old Metso's EUR 25-35. No split in the range. Saxo's bars for the same dates: **not available yet**.

## 4. Insider transactions

`insiders::build` over the MFN feed gives 40 transactions for 2023-2026 (10 per year), from 80 releases (40 Finnish + 40 English copies). Nasdaq's feed lists exactly **80** "Managers' Transactions" items for the same period, 40 + 40, with the same person and date for each. Every one of the 40 English releases was also fetched from Nasdaq's release page and compared on volume, transaction date and unit price: **39 identical, 1 different** (2023-03-16, Pekka Vauramo: Nasdaq's English text says the transaction date is 2022-03-15; the Finnish copy, which is what the pipeline stores because the English copy is in an unparsed layout, says 2023-03-15; [NEX-214](https://linear.app/nexusboard/issue/NEX-214)).

2025 and 2026, as `insider_filings` should hold them (type GRANT = transaction code `A`):

| # | Released (UTC) | Transaction date | Person | Role | Type | Volume | Unit price | Nasdaq release page (volume, date, price) |
|---|---|---|---|---|---|---|---|---|
| 1 | 2025-03-20 13:15 | 2025-03-20 | Sami Takaluoma | Chief Executive Officer | GRANT | 10426 | 0.00 EUR | identical |
| 2 | 2025-04-28 13:30 | 2025-04-25 | Anders Svensson | Member of the Board | GRANT | 3110 | 0.00 EUR | identical |
| 3 | 2025-04-28 13:30 | 2025-04-25 | Arja Talma | Member of the Board | GRANT | 4254 | 0.00 EUR | identical |
| 4 | 2025-04-28 13:30 | 2025-04-25 | Brian Beamish | Member of the Board | GRANT | 1673 | 0.00 EUR | identical |
| 5 | 2025-04-28 13:30 | 2025-04-25 | Eriikka Söderström | Member of the Board | GRANT | 3582 | 0.00 EUR | identical |
| 6 | 2025-04-28 13:30 | 2025-04-25 | Kari Stadigh | Member of the Board | GRANT | 7915 | 0.00 EUR | identical |
| 7 | 2025-04-28 13:30 | 2025-04-25 | Klaus Cawén | Member of the Board | GRANT | 4313 | 0.00 EUR | identical |
| 8 | 2025-04-28 13:30 | 2025-04-25 | Niko Pakalén | Member of the Board | GRANT | 3346 | 0.00 EUR | identical |
| 9 | 2025-04-28 13:30 | 2025-04-25 | Reima Rytsölä | Member of the Board | GRANT | 3684 | 0.00 EUR | identical |
| 10 | 2025-04-28 13:30 | 2025-04-25 | Terhi Koipijärvi | Member of the Board | GRANT | 3582 | 0.00 EUR | identical |
| 11 | 2026-03-20 12:40 | 2026-03-19 | Sami Takaluoma | Chief Executive Officer | GRANT | 23201 | 0.00 EUR | identical |
| 12 | 2026-04-24 13:00 | 2026-04-23 | Anders Svensson | Member of the Board | GRANT | 2071 | 0.00 EUR | identical |
| 13 | 2026-04-24 13:00 | 2026-04-23 | Arja Talma | Member of the Board | GRANT | 2642 | 0.00 EUR | identical |
| 14 | 2026-04-24 13:00 | 2026-04-23 | Eriikka Söderström | Member of the Board | GRANT | 2217 | 0.00 EUR | identical |
| 15 | 2026-04-24 13:00 | 2026-04-23 | Kari Stadigh | Member of the Board | GRANT | 2476 | 0.00 EUR | identical |
| 16 | 2026-04-24 13:00 | 2026-04-23 | Klaus Cawén | Member of the Board | GRANT | 2668 | 0.00 EUR | identical |
| 17 | 2026-04-24 13:00 | 2026-04-23 | Matts Rosenberg | Member of the Board | GRANT | 2283 | 0.00 EUR | identical |
| 18 | 2026-04-24 13:00 | 2026-04-23 | Niko Pakalén | Member of the Board | GRANT | 1035 | 0.00 EUR | identical |
| 19 | 2026-04-24 13:00 | 2026-04-23 | Petra Sundström | Member of the Board | GRANT | 2217 | 0.00 EUR | identical |
| 20 | 2026-04-24 13:00 | 2026-04-23 | Terhi Koipijärvi | Member of the Board | GRANT | 2217 | 0.00 EUR | identical |

- **2026-03-20:** Sami Takaluoma, Chief Executive Officer, **GRANT, 23 201 shares at 0.00 EUR**, transaction date 2026-03-19. As expected.
- **2026-04-24:** **9 distinct people**, all board members, GRANT at 0.00 EUR on 2026-04-23: Arja Talma 2 642, Niko Pakalén 1 035, Anders Svensson 2 071, Terhi Koipijärvi 2 217, Kari Stadigh 2 476, Matts Rosenberg 2 283, Eriikka Söderström 2 217, Klaus Cawén 2 668, Petra Sundström 2 217. Each in a Finnish and an English release, stored once. (Their other values are in the table; the volumes were also read from Nasdaq's pages for Talma, Pakalén and Rosenberg as a first check, and for all 40 in the full comparison.)
- **No BUY at price 0:** 0 of 40 (all 40 rows of 2023-2026 are GRANT at 0.00 EUR).
- **Unique key.** No person has two trades on one day with the same volume and code, so `UNIQUE (person_id, ticker, transaction_date, shares, transaction_code)` collapses nothing in Metso's 232-transaction history.
- **Parse errors.** `nordic_mar_parse_errors_total{exchange_mic="XHEL"}`: not available, a CronJob has no scrape endpoint and no metrics recorder is installed; the same count is in the `nordic-reports` end-of-run log line (`mar_parse_errors`). For the whole history (2016-2026, 319 releases) the dry run reports **9 releases not parsed, 0 of them since 2023**:

| Release | Reason |
|---|---|
| [2022-06-30 fi, Pekka Vauramo](https://mfn.se/cis/a/metso-outotec/metso-outotec-oyj-johtohenkiloiden-liiketoimet-pekka-vauramo-902b2359) | `invalid transaction date ""` (the release has an empty date and volume) |
| [2022-06-30 en, Pekka Vauramo](https://mfn.se/cis/a/metso-outotec/metso-outotec-oyj-metso-outotec-corporation-managers-transactions-pekka-vauramo-c2d80c6e) | `missing label: Reference number` (wrapped-label layout) |
| [2021-06-08 fi, Lauri ja Lasse Reitzin säätiö](https://mfn.se/cis/a/metso-outotec/metso-outotec-oyj-johtohenkiloiden-liiketoimet-lauri-ja-lasse-reitzin-saatio-564e2b1e) | close associate without the manager block (corrected the next day) |
| [2021-06-08 en, Lauri ja Lasse Reitzin säätiö](https://mfn.se/cis/a/metso-outotec/metso-outotec-oyj-metso-outotec-corporation-managers-transactions-lauri-ja-lasse-reitzin-saatio-3f2b8000) | same |
| [2020-07-03 en, Reitzin säätiö](https://mfn.se/cis/a/metso-outotec/metso-outotec-oyj-metso-outotec-corporation-managers-transactions-lauri-ja-lasse-reitzin-saatio-11207dab) | same, no manager block |
| [2020-07-03 en, Solidium Oy](https://mfn.se/cis/a/metso-outotec/metso-outotec-oyj-metso-outotec-corporation-managers-transactions-solidium-oy-4afec9f2) | same |
| [2020-07-03 fi, Solidium Oy](https://mfn.se/cis/a/metso-outotec/metso-outotec-oyj-johtohenkiloiden-liiketoimet-solidium-oy-ecf4ad7f) | same |
| [2020-07-03 fi, Reitzin säätiö](https://mfn.se/cis/a/metso-outotec/metso-outotec-oyj-johtohenkiloiden-liiketoimet-lauri-ja-lasse-reitzin-saatio-f09f647e) | same |
| [2017-05-12 fi, Matti Alahuhta](https://mfn.se/one/a/outotec/outotec-oyj-johtohenkiloiden-liiketoimet-matti-alahuhta-5bea31b1) | `missing label: Reference number` (old template) |

A further 18 English copies in the wrapped-label layout (2020-08-10, 2021-04-28, 2023-03-16) do not parse either, but their Finnish twins do, so no transaction is lost and they are not counted as errors. One consequence: those rows carry the Finnish role text (`Toimitusjohtaja` instead of `Chief Executive Officer`).

## SQL for the deployed system

Read-only, signal's database (`backtest`). Compare with the tables above. Every query below was run against the production schema on 2026-10-29 and is valid; today they return no rows (or 0).

```sql
-- R1: every Metso report row since 2023 (UTC and Helsinki)
SELECT to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI')              AS utc,
       to_char(occurred_at AT TIME ZONE 'Europe/Helsinki', 'YYYY-MM-DD HH24:MI') AS helsinki,
       event_type, status, report_kind, fiscal_period, lang, left(description, 70) AS title
FROM special_events
WHERE ticker = 'METSO' AND exchange_mic = 'XHEL' AND source = 'mfn' AND occurred_at >= '2023-01-01'
ORDER BY occurred_at;                       -- expect the 17 rows of section 1, each once, lang = 'en'

-- R2: no Neles, and what is before 2020-07-01
SELECT count(*) FROM special_events WHERE ticker = 'METSO'
  AND (description ILIKE '%neles%' OR (occurred_at < '2020-07-01' AND description ILIKE '%metso%'));   -- expect 0
SELECT count(*), min(occurred_at), max(occurred_at) FROM special_events
 WHERE ticker = 'METSO' AND occurred_at < '2020-07-01' AND description NOT ILIKE '%outotec%';          -- expect 0

-- R3: twins merged: one row per instant and type
SELECT occurred_at, event_type, count(*) FROM special_events WHERE ticker = 'METSO' AND status = 'occurred'
GROUP BY 1, 2 HAVING count(*) > 1;          -- expect no rows

-- R4: upcoming
SELECT expected_date, expected_time, calendar_label, fiscal_period,
       to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI') AS utc
FROM special_events WHERE ticker = 'METSO' AND status = 'scheduled' ORDER BY occurred_at;
                                            -- expect 2027-02-19 07:00, 04-19 06:00, 07-22 06:00, 10-25 06:00

-- X1: reactions of the section 3 reports
SELECT to_char(occurred_at AT TIME ZONE 'Europe/Helsinki', 'YYYY-MM-DD HH24:MI') AS helsinki,
       fiscal_period, release_session, reaction_date, base_date, base_close, reaction_close,
       move_pct, data_status, needs_review
FROM special_events
WHERE ticker = 'METSO' AND exchange_mic = 'XHEL' AND status = 'occurred' AND report_kind IS NOT NULL
ORDER BY occurred_at;

-- X2: sanity
SELECT data_status, count(*) FROM special_events WHERE ticker = 'METSO' AND report_kind IS NOT NULL
  AND status = 'occurred' GROUP BY 1 ORDER BY 1;
SELECT * FROM special_events WHERE ticker = 'METSO' AND needs_review;                    -- expect no rows
SELECT min(move_pct), max(move_pct) FROM special_events WHERE ticker = 'METSO' AND data_status = 'ok';
                                            -- expect about -8.39 and +12.80 for 2023+; wider before

-- X3: continuity in Saxo's bars
SELECT date, close FROM daily_bars WHERE ticker = 'METSO' AND exchange_mic = 'XHEL'
  AND date BETWEEN '2020-06-26' AND '2020-07-02' ORDER BY date;     -- compare with section 3

-- I1: insiders by year and type (transaction_code: P buy, S sell, A grant, J other)
SELECT date_part('year', f.transaction_date) AS year, f.transaction_code, count(*)
FROM insider_filings f WHERE f.ticker = 'METSO' GROUP BY 1, 2 ORDER BY 1, 2;
SELECT f.transaction_date, f.filing_date, p.filer_name, f.filer_role, f.transaction_code, f.shares, f.price_per_share
FROM insider_filings f JOIN persons p ON p.id = f.person_id
WHERE f.ticker = 'METSO' AND f.filing_date >= '2025-01-01' ORDER BY f.filing_date, p.filer_name;   -- 20 rows
SELECT count(*) FROM insider_filings WHERE ticker = 'METSO' AND transaction_code = 'P' AND price_per_share = 0;  -- expect 0
```

## Findings

| | Finding | Where |
|---|---|---|
| 1 | Fi/en twins published seconds apart (2012-2019) are not merged: 24 duplicate pairs, 14 from 2015 | [NEX-212](https://linear.app/nexusboard/issue/NEX-212) |
| 2 | A correction notice of a non-report (the bond issue) becomes an orphan `report_correction` row | [NEX-213](https://linear.app/nexusboard/issue/NEX-213) |
| 3 | English managers' transaction releases in a wrapped-label layout are not parsed; the row takes the Finnish copy (role in Finnish); the 2022-06-30 release is lost; one English release has a wrong year | [NEX-214](https://linear.app/nexusboard/issue/NEX-214) |
| 4 | Q3 report days are Metso's ex-dividend days: the reaction includes the dividend drop (1.2-1.9 points) | [NEX-215](https://linear.app/nexusboard/issue/NEX-215) |
| 5 | Nothing is deployed for Finland yet; `backtest.companies` has a METSO row without mapping | [What is missing](#what-is-missing-before-the-system-can-be-checked) |

## Pass criteria

Checks that need only the independent sources and the real code on real data: **9 of 9 pass**.

| # | Check | Result |
|---|---|---|
| P1 | Every interim, half-year and full-year report since 2023 appears exactly once vs Nasdaq's feed | pass (16 of 16; plus the orphan correction, finding 2) |
| P2 | No Neles item; pre-2020 rows are Outotec's | pass (0 of 124; 59 of 59) |
| P3 | Upcoming dates match Metso's announced dates, times read as Stockholm time | pass (4 of 4) |
| P4 | At least 8 reports have expected sessions, days and moves from an independent source, including Q3 2026 (pre-open, r 2026-10-22, b 2026-10-21) | pass as expected values (16; Q3 2026: 16.310 to 16.070, -1.47 %); single source (Yahoo) |
| P5 | Continuity across 2020-06-30 / 2020-07-01 | pass (4.912 to 4.946, +0.69 %; Yahoo) |
| P6 | 2025-2026 managers' transaction releases map to the right rows (person, type, volume, price) | pass (20 of 20; 39 of 40 since 2023, the other is the issuer's typo) |
| P7 | Spot checks: 2026-03-20 and 2026-04-24 | pass |
| P8 | No BUY at price 0 | pass (0 of 40) |
| P9 | Failed releases listed with URL and reason | pass (9 listed, 0 since 2023) |

Checks that need the deployed system: **0 of 8 run** (one does not apply) (blocked, see [What is missing](#what-is-missing-before-the-system-can-be-checked)).

| # | Check | Result |
|---|---|---|
| S1 | R1: the actual report rows since 2023 | blocked |
| S2 | R2/R3: no Neles, no duplicates, in the table | blocked |
| S3 | R4: the stored upcoming rows | blocked |
| S4 | X1: signal's closes and moves vs the independent closes (at least 8 reports) | blocked |
| S5 | A report next to a holiday in the table | not applicable (Metso has none); the unit tests cover it |
| S6 | X2: counts by `data_status`, `needs_review`, min/max `move_pct` | blocked |
| S7 | X3: Saxo's closes across the merger date | blocked |
| S8 | I1: the actual `insider_filings` rows, and the BUY-at-0 query on the table | blocked |
| S9 | Grafana Nordic Reports / Event Log panels (EUR, red for negative) | blocked |

## Reproducing

- **Releases:** `GET https://api.news.eu.nasdaq.com/news/query.action?globalGroup=companyNews&displayLanguage=en&timeZone=CET&company=Metso%20Corporation&limit=100&start=<n>&dir=DESC` pages through the feed (`cnsCategory`, `published`, `messageUrl`); `messageUrl` is the release page.
- **MFN:** `https://mfn.se/all/a/metso-2.json?limit=48&offset=<n>` (sequential, at most one request per second) and `https://mfn.se/a/metso-2`.
- **Prices:** the Yahoo URL above with `period1`/`period2`, `interval=1d`, `events=div`.
- **The pipeline:** a small program calling `classify_item`, `release_event` and `insiders::build` on the MFN items (the same functions `nordic-reports` uses), with the reports merged by (instant, type) as signal does.
