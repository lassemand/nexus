# Registering a Nordic company

Companies are registered at runtime; nothing in code, migrations or docs is
specific to one company. A company row (`ticker`, `exchange_mic`, `currency`)
is created from the registration: `nexus register <yahoo-symbol>` publishes to
`company.tickers`, and `pdmr`, `nordic-reports` and `nordic-calendar` copy new
registrations into `companies` when they start (`chronicle_lib::registry`).

The Nordic report sources identify companies by their MFN slug, not by ticker,
so a company row also carries source mappings. `nexus register` looks them up
and sends them with the registration (see *Source mappings* below); the sync
sets them on the chronicle `companies` table:

| Column | Meaning |
|---|---|
| `name` | Display name used by the dashboard, free text |
| `mfn_slug` | Slug of the company page on MFN (`https://mfn.se/a/<slug>`). Must match `^[a-z0-9][a-z0-9-]*$` and be unique |
| `fi_issuer` | Issuer name exactly as Finansinspektionen's insider register spells it. Used by `pdmr` for PDMR (insider) transactions (Swedish listings only). Unique |
| `isin` | Sent by `nexus register`, else filled in by `nordic-reports` (see below); set by hand only when MFN lists several |

All are nullable. A registration that leaves a mapping empty never clears it,
and a later registration with a value replaces it, so running `nexus register`
again for a registered company fills in or corrects its mappings. The row (and
its mappings) appear at the next `pdmr`, `nordic-reports` (daily at 22:00) or
`nordic-calendar` run, or start one by hand.

## Source mappings

For a listing on `FNSE`, `XSTO`, `XCSE`, `FNDK`, `XHEL` or `FNFI`
(`model::asset::mic::NORDIC`), `nexus register`:

1. Searches MFN's company search (`https://mfn.se/search/companies?query=<TICKER>`)
   and keeps the companies whose `tickers` contain `<MIC>:<TICKER>`, compared
   uppercase with spaces and hyphens removed (MFN's `XCSE:<TICKER> B` matches
   the registry's `<TICKER>-B`). Exactly one: its `slug` and `name` are taken.
   Several, or none matching while the search found companies: it lists them
   and asks which one (`0` for none). The chosen company's ISIN is sent too
   when it lists exactly one valid ISIN.
2. For Swedish listings, looks the MFN name up in FI's issuer autocomplete
   (below). One result is taken; several are listed and asked about.

Each line of the result says what was set, e.g.
`registered (XCSE / DKK), MFN <slug> (<name>)`. Without a terminal (a script)
an ambiguous lookup is left empty and its options are printed. A failed lookup
never fails the registration. `--mfn-slug <slug>` and `--fi-issuer <name>` (one
ticker per command) skip the lookup. A mapping the table rejects (a slug or
issuer another company already has) is logged by the sync and skipped.

## Choosing the symbol and MIC

`nexus register` looks the symbol up on Yahoo Finance and takes the exchange and
currency from it. The registry keeps the Yahoo symbol without its suffix
(`<TICKER>.ST` → `<TICKER>`).

- Use the Yahoo symbol of the Nordic listing, with its suffix: `.ST` for
  Stockholm, `.CO` for Copenhagen. A bare symbol resolves to whatever Yahoo
  lists under it, often a US listing of the same company. Share classes are part
  of the symbol (`<TICKER>-A.CO`, `<TICKER>-B.CO`).
- Yahoo reports Copenhagen as `CPH` for both markets, so a First North Denmark
  company needs `--mic FNDK`. Likewise `--mic XSTO` for a Stockholm main-market
  share (`STO` alone gives `FNSE`). The MIC must be in the same country as
  Yahoo's exchange; `--mic FNDK` on a `.ST` symbol is an error.
- Check the result line, e.g. `registered (XCSE / DKK), MFN …`. A wrong
  listing is withdrawn with `nexus unregister` (below).

## Finnish companies

Register with the Yahoo symbol and its `.HE` suffix:

```bash
nexus register <TICKER>.HE            # Nasdaq Helsinki  -> XHEL / EUR
nexus register <TICKER>.HE --mic FNFI # First North Finland -> FNFI / EUR
```

Yahoo reports `HEL` for both markets, so a First North Finland company needs
`--mic FNFI` (the same way `--mic FNDK` is needed in Copenhagen). The MIC must be
in Finland: `--mic XCSE` on a `.HE` symbol is an error. Prices stay in EUR.
Share classes have no separator in Finland (`<TICKER>`, not `<TICKER>-B`), so
MFN's `XHEL:<TICKER>` matches the registry ticker as is.

**Same-name trap.** MFN can hold two different companies with the same name.
Metso is the example: `metso` is the old Metso Oyj (renamed Neles in 2020, since
merged into Valmet; ISIN FI4000440664, `XHEL:NELES`), while `metso-2` is today's
Metso (ISIN FI0009014575, `XHEL:METSO`). Both ISINs are valid, so an ISIN check
does not catch the mix-up; the `XHEL:METSO` ticker does, which is why the lookup
matches on `<MIC>:<TICKER>` and only takes a company that lists it. If the search
finds only companies that do not list the ticker, `nexus register` does not pick
one: it lists them and asks (`0` for none, then pass `--mfn-slug`). Check the
result line: `MFN <slug> (<name>), ISIN <isin>`.

Check the mapping in both databases (chronicle's and signal's `companies`, after
the registry sync):

```sql
SELECT ticker, exchange_mic, mfn_slug, isin FROM companies WHERE exchange_mic IN ('XHEL','FNFI');
```

Finland has no insider register like Sweden's, so there is no FI issuer. Finnish
insider trades come from the issuers' own MFN releases (a later task), not from a
regulator register.

`nordic-reports`, `nordic-calendar` and `nordic-live` take Finnish companies like
the others (`NORDIC_MICS` is `model::asset::mic::NORDIC`). What is specific to them:

- **Every release comes twice**, in Finnish and English (`properties.lang`), with the
  same instant and different MFN ids. Signal merges the pair into one
  `special_events` row and keeps the English one; a Finnish release whose English
  copy never arrives stays as a report.
- **Titles.** `osavuosikatsaus` (interim), `puolivuosikatsaus` (half year),
  `tilinpäätöstiedote` (the year-end results; English "Financial Statements
  Review"), `liiketoimintakatsaus` (business review). `vuosikertomus` is the annual
  report *document*, not a results release. The period in the title wins over the
  word: an "osavuosikatsaus 1. tammikuuta - 31. joulukuuta" is a full-year report.
  Managers' transactions (`johtohenkilöiden liiketoimet`), buy-backs, flagging
  notices and meeting notices are never reports. They arrive in bursts (one person
  per release, many at the same instant), which is why twins are only merged for
  reports.
- **A renamed company keeps its ISIN, not its MFN slug.** Metso's feed carries its
  earlier releases under the `outotec` and `metso-outotec` entities. With the
  company's ISIN in `companies.isin`, `nordic-reports` publishes those too. A
  namesake (the old Metso, ISIN FI0009007835; Neles, FI4000440664) is never ours.
- **Wrong company.** If the first item of the feed does not list the registry's
  `MIC:TICKER` or ISIN, `nordic-reports` stops for that company (`mapping points at
  the wrong company, nothing published`, the company counts as failed) instead of
  publishing another company's releases. `nordic-live` ignores such items.
- **Calendar times are Stockholm time**, also for Finnish companies: MFN's
  `2027-10-25 08:00` is 06:00Z, which is 09:00 in Helsinki.

### Finnish insider transactions

Finland has no insider register, so each issuer's MAR Art. 19 release on MFN is the
source. `nordic-reports` routes a Finnish company's managers' transaction releases
(headline "Managers' transactions" / "Johtohenkilöiden liiketoimet", or the form text
"Person subject to the notification requirement" / "Ilmoitusvelvollinen") to
`chronicle_lib::nordic::insiders` instead of the report classifier, parses them
(`mar_release`) and publishes one `UnifiedInsiderTransaction` per transaction to
`insider.transactions` (`source_registry = FIN_MAR`). Signal stores them in
`insider_filings`. Swedish companies are not touched (FI's register covers them,
`pdmr`), Danish insiders are not collected.

- Each release comes in Finnish and English with the same MAR reference number; the
  English copy is read, the Finnish one only when no English copy with that reference
  was. Ids are `FIN_MAR:{reference}:{index}` and re-sending is harmless.
- `published_date` is the release date in Helsinki; `ticker` and `exchange_mic` come
  from the registry. Releases before MAR (2016-07-03) are ignored.
- The feed cursor moves only after the company's report events **and** insider
  messages were acknowledged. A release that does not parse is logged (`news_id`,
  `url`, error) and counted (`nordic_mar_parse_errors_total`,
  `nordic_mar_suspicious_zero_price_total`, both per `exchange_mic`; the totals are
  also in the end-of-run log line) but does not hold the cursor back.
- A zero price on a BUY or SELL is never published; incentives and grants at 0.00 EUR
  are normal and stored as `GRANT` (code `A`).

Re-read a company's history for insiders only (no report events, the cursor is
neither read nor moved, safe to repeat):

```bash
kubectl --context orbstack -n chronicle create job nordic-insiders-now --from=cronjob/nordic-reports \
  --dry-run=client -o json | jq '.spec.template.spec.containers[0].env += [
    {"name":"NORDIC_INSIDERS_ONLY","value":"true"},
    {"name":"NORDIC_TICKER","value":"<TICKER>"},
    {"name":"NORDIC_MIC","value":"XHEL"},
    {"name":"NORDIC_INSIDERS_FROM","value":"2016-07-03"}]' \
  | kubectl --context orbstack apply -f -
```

(`nordic-reports --insiders-only --ticker <TICKER> --mic XHEL --from 2016-07-03` is the
same thing.) Check in signal's database:

```sql
SELECT date_part('year', f.transaction_date) AS year, f.transaction_code, count(*)
FROM insider_filings f WHERE f.ticker = '<TICKER>' GROUP BY 1, 2 ORDER BY 1, 2;
```

Known limits: `insider_filings` is `UNIQUE (person_id, ticker, transaction_date, shares,
transaction_code)`, so two trades of one person on one day with the same volume and
code keep only the first (the price differs); a corrected (amended) notification adds a
row instead of replacing the first; money is `DOUBLE PRECISION`.

## Unregistering

`nexus unregister <yahoo-symbol>` (or `nexus unregister <TICKER> --mic <MIC>`,
which skips the Yahoo lookup and also works for a delisted ticker) publishes the
same message with `unregister` set. The topic is replayed in order, so the last
message for a listing decides. The next `pdmr`, `nordic-reports` or
`nordic-calendar` run deletes the `companies` row together with its
`mfn_ingest_state` and `saxo_daily_bars_state` rows, and `saxo-stream` stops
fetching it from its next sweep. The US jobs (`chronicle`, `market`,
`earnings`, `filings`) drop it on their next run. The mappings go with the row;
registering the company again looks them up again. signal deletes its own
`companies` row straight away and skips the ticker's new events. Data already
stored (`special_events`, `daily_bars`, ...) is kept.

## Checking the MFN slug

The slug is the last path segment of the company's page on
[mfn.se](https://mfn.se): `https://mfn.se/a/<slug>`. To check the mapping, open `https://mfn.se/all/a/<slug>.json`: the items'
`author.tickers` should contain `<exchange_mic>:<ticker>`, compared uppercase
with spaces and hyphens removed (MFN's `XCSE:<TICKER> B` matches the registry's
`<TICKER>-B`), and, when `companies.isin` is set, `author.isins` should contain
it. Otherwise `nordic-reports` logs `mapping may point at the wrong company`
with both values.

## ISIN

The ISIN is the one identifier MFN, Saxo and Yahoo share whatever they call the
ticker; `saxo-stream` resolves instruments by it, and falls back to ticker text,
which Saxo does not always know. `nexus register` sends it when the MFN company
lists exactly one valid ISIN. Otherwise `nordic-reports` reads it from MFN's
`author.isins` the first time it fetches a company's feed (so after `mfn_slug`
is set) and stores it in `companies.isin` when that is empty; it never
overwrites one. When MFN lists several valid ISINs (several share classes) none
is stored; set it by hand then, from `https://mfn.se/all/a/<slug>.json` →
`author.isins`.

## FI issuer name

Swedish listings only. `nexus register` queries FI's issuer autocomplete with
the MFN name; to look one up yourself, use part of the company name (not the
ticker) and copy the current name verbatim:
`https://marknadssok.fi.se/Publiceringsklient/sv-SE/AutoComplete/H%C3%A4mtaAutoCompleteListaFull?sokfunktion=Insyn&falt=Utgivare&sokterm=<part-of-name>`.
Without `fi_issuer`, pdmr searches with the ticker, which only works when the
ticker is part of the issuer name. **Danish insider trades are not covered**:
there is no FI-style register for them, deliberately deferred.

## SQL

Only needed when a lookup found nothing. Prefer `nexus register <yahoo-symbol>
--mfn-slug <slug>`; by hand, on the chronicle database:

```sql
UPDATE companies
   SET name      = '<display name>',
       mfn_slug  = '<slug>',
       fi_issuer = '<FI issuer name>'   -- Swedish listings only
 WHERE ticker = '<TICKER>' AND exchange_mic = '<MIC>';
```

Check that one row was updated (`UPDATE 1`). `UPDATE 0` means the company is
not registered yet (or has not been synced into `companies`).

## Report classification

`nordic-reports`, `nordic-calendar` and `nordic-live` need only the `mfn_slug`.
Report times are stored in Stockholm local time, which equals Copenhagen's; a
release after the close is handled by the reaction rules in signal. Notices that
only look like reports are never published, whatever MFN tags them: PDMR
notices ("persons discharging managerial responsibilities"), share buy-backs,
voting-rights and major-shareholder notices (`HARD_NON_REPORT_WORDS` in
`nordic/classify.rs`). Some companies tag their year-end results
`sub:report:annual` only, so an annual-tagged item counts as year-end results
when it is not the annual report document, names exactly one year and has a
results word (sales, profit, revenue, results, financial report/statement).

## Trading holidays (Sweden, Denmark, Finland)

`trading_holidays` (signal DB) is seeded per **country**, not per MIC:
`FNSE`/`XSTO` use `SE`, `XCSE`/`FNDK` use `DK`, `XHEL`/`FNFI` use `FI` (`model::nordic`, via `alpha::calendar::country_for_exchange`).
Session hours per MIC are in the `exchanges` table (Copenhagen: 09:00-17:00
`Europe/Copenhagen`, no half days; Stockholm: 09:00-17:30, 13:00 half-day close;
Helsinki: 10:00-18:30 `Europe/Helsinki`, no half days; prices in EUR).

Denmark's calendar adds closures Nager.Date may not list: the Friday after
Ascension, 5 June, 24 and 31 December (fully closed), and Great Prayer Day
(Store Bededag) for years up to and including 2023.

Finland's calendar is computed, not read from Nager.Date (whose FI list has
Saturday-only holidays and lacks 24 and 31 December): 1 Jan, 6 Jan, Good Friday,
Easter Monday, 1 May, Ascension Day, Midsummer Eve (the Friday 19-25 June),
6 Dec, 24, 25, 26 and 31 Dec. Only weekdays are stored, so 6 and 26 December
count only when they fall on one. Source: Nasdaq's holiday schedule at
https://www.nasdaq.com/european-market-activity/trading-hours.

Seed from the Mac mini (`DATABASE_URL` set to the signal DB):

```bash
nexus calendar sync --country DK --from-year 2015 --to-year 2027
# or one year at a time:
nexus calendar sync --country DK --year 2026
```

Confirm (expect 11; 26 Dec 2026 is a Saturday):

```sql
SELECT count(*) FROM trading_holidays
WHERE country = 'DK' AND date BETWEEN '2026-01-01' AND '2026-12-31';
```

## Where reports are stored (`special_events`)

Nordic reports are stored **only by signal**, in `backtest.special_events`, with
the other special events; every dashboard reads them from there. chronicle
fetches, classifies and publishes `SpecialEvent`s on `special.events`, keyed
`"{ticker}:{exchange_mic}"` so one company's messages stay in order. chronicle
keeps only its read position, `mfn_ingest_state` (feed cursor, calendar hash).

| `event_type` | What | Notes |
|---|---|---|
| `quarterly_report` | Interim report (`report_kind = interim`) | |
| `annual_report` | Year-end report (`report_kind = annual`) | |
| `report_correction` | `Correction:` / `Rättelse:` items, and same-day re-sends | signal links the report to its latest correction within 7 days (`superseded_by`) |
| `report_calendar` | Snapshot of a company's upcoming reports | not a row itself; applied to `status = 'scheduled'` rows |

`status` is `scheduled` (from the calendar; `occurred_at` is the expected
instant, `expected_date`/`expected_time` are Europe/Stockholm local and the time
is MFN's estimate) or `occurred` (released; `occurred_at` is MFN's
`publish_date`, UTC). `description` is the MFN headline, or the calendar label
for scheduled rows.

signal's rules (`signal/src/reports.rs`):

1. An MFN item (`(source, source_id)` = `("mfn", news_id)`) is stored once; re-deliveries refresh its display fields.
2. The sv/en twin of a release (same company, instant and type) is merged into one row; English wins.
3. A report (not a correction) fills the closest **scheduled** row of the company whose `expected_date` is within ±7 days of the release's Stockholm date (tie: the earlier date), keeping the expected fields.
4. Otherwise a new `occurred` row.
5. A report on the same Stockholm date as an earlier report of the company becomes a `report_correction`; the earliest instant is the release the market reacted to.
6. A page calendar snapshot is diffed against the company's future page rows (see "Calendar sources" below); an entry matching an occurred report without expected fields (±7 days) fills them instead. Past scheduled rows stay ("expected, not seen"). An empty snapshot clears the future ones.

A database error in signal is retried with backoff before the offset is
committed, so no report event is lost.

## Released reports (`nordic-reports`)

The `nordic-reports` chronicle binary (CronJob in `infra/charts/chronicle/chronicle/`,
daily at 22:00 `Europe/Stockholm`) reads every
`FNSE`/`XSTO`/`XCSE`/`FNDK` company (`chronicle_lib::nordic::NORDIC_MICS`) with an `mfn_slug`, pages MFN's JSON news feed
`https://mfn.se/all/a/<slug>.json` newest first (following `next_url`), keeps
items authored by that slug, classifies them and publishes reports and
corrections. Other items (business news, calendar notices, report documents)
are not published.

- Incremental: paging stops once a page reaches items older than `feed_cursor − 1 day` (the overlap catches items MFN lists late; re-sends are harmless). A company with no cursor, or `--backfill`, pages to the end.
- The cursor moves to the newest published item's `publish_date` only after every message for the company was acknowledged by Kafka; a failed publish leaves it, and the next run re-sends.
- A calendar notice newer than the cursor at the start of the run triggers that company's calendar refresh at the end of the run.
- `report_kind` is `interim` or `annual` for periodic reports. Rule: category tags `sub:report:interim*` / `sub:report:annual` decide; otherwise the headline needs report wording (`report`, `interim`, `half-year`, `year-end`, `annual`, `rapport`, …) or a quarter mention (`Q1`–`Q4`, `quarter`, `kvartal`) without business-news wording (`order intake`, `guidance`, `contract`, `agreement`, `secures`, `signs`, `market update`).

## Upcoming reports (`nordic-calendar`)

The `nordic-calendar` binary (daily CronJob, 06:30 `Europe/Stockholm`) reads
every company with an `mfn_slug`, fetches the public HTML page
`https://mfn.se/a/<mfn_slug>` and parses its "Kalender" table
(`table.table-calender`). Only the HTML page is used (MFN's `robots.txt`
disallows the JSON/RSS/XML feeds): one request per company per day, at least 2 s
apart, 20 s timeout, `429` honours `Retry-After`.

- Rows without `grey-background` are upcoming. Report labels (`Kvartalsrapport`, `Delårsrapport`, `Halvårsrapport`, `Bokslutskommuniké`, `Årsredovisning`, `Interim report`, `Quarterly report`, `Half-year report`, `Year-end report`, `Annual report`) become entries with `report_kind`, `fiscal_period` (`2026-Q3` / `2026`), `calendar_label` (original text), `expected_date` and the estimated `expected_time` (empty when MFN shows `-`). Non-report events (AGM, ex-dividend date, …) are ignored; unknown upcoming labels are logged at `warn`.
- The parsed calendar is published as one `report_calendar` snapshot (possibly empty), **unless** its hash equals `mfn_ingest_state.calendar_hash` (the last snapshot published); `--force` publishes regardless. The hash is stored after Kafka acknowledged the snapshot.
- **A parse failure publishes nothing** and keeps the stored hash; it is logged at `error` and the job exits non-zero.

## Calendar sources (chronicle)

`nordic-reports` and `nordic-calendar` emit two independent calendar sources on `special.events`, keyed `ticker:exchange_mic`; they never merge or rank them (signal does, see below).

- **The MFN page** (`nordic-calendar`, and `nordic-reports` after a calendar announcement): one `report_calendar` snapshot, only when its hash changed.
- **Calendar releases** (`nordic-reports`): for every feed item that is a financial-calendar release (tag `sub:ci:calendar` or a calendar-update headline; `parse_calendar_release`), one `report_calendar_release` event with `source_id` = the MFN news id, `url`, `lang`, `tags`, `description` = the title, `occurred_at` = the publish instant and `calendar` = its dated report lines (`fetched_at` = the publish instant). Both language twins are sent, each with its own news id, and a release with no readable dates is sent with no entries. Release events go out while the feed is read, before the calendar-announcement refresh of the page snapshot. Times in a release are issuer-local and are converted to Stockholm time (Helsinki 08:00 becomes 07:00); date-only entries stay date-only. chronicle keeps no state for releases: signal upserts per news id, so re-sending is harmless.
- **`nordic-calendar --scan-releases[=<months>]`** (default 18) first pages each mapped company's news feed back that far and sends a `report_calendar_release` for every calendar release found, then runs the normal page refresh. Run it once after signal can handle the event type, and whenever a company is registered late.

## Calendar sources (signal)

chronicle only emits source data; signal decides which source wins. No extra tables: the sources live in `special_events` as scheduled rows.

| Event | Rows | Timestamp (`calendar_fetched_at`) |
|---|---|---|
| `report_calendar` (the MFN page) | `source = 'mfn'` | the snapshot's `fetched_at`, **per report** |
| `report_calendar_release` (one financial-calendar release; `source_id` = MFN news id, `url`, `lang`, `description` = title) | `source = 'mfn_release'`, `url` = the release, `source_id` stays NULL | the release's publish instant |

- A page snapshot is a **diff per report**: a report whose date did not move keeps its row and timestamp, a moved or new report gets the snapshot's `fetched_at`, a future page row missing from the snapshot is deleted. A snapshot older than the newest page row's timestamp is ignored (release rows do not count).
- A release adds rows of its own. Language twins (same publish instant) count once, English preferred; a later release replaces an earlier release's row for the same report; an older one is dropped. Re-delivery gives the same rows.
- **Same report** = same `report_kind` and, when both periods are known, the same `fiscal_period`; otherwise the same kind within ±7 days. Same report and same date: the page row wins (it has a time) and the release fills a missing period. **Different dates: the row with the newer `calendar_fetched_at` wins** (a tie goes to the page) and is logged at `warn`.
- The loser keeps its row and points at the winner with `superseded_by` (on scheduled rows; on occurred rows the same column still means "corrected by"). The links are cleared and recomputed after every page snapshot or release. **Readers use scheduled rows with `superseded_by IS NULL`** (`source IN ('mfn','mfn_release')`). When the released report arrives it fills the winning row and the losing rows are deleted.

## After deploying NEX-171

chronicle's migration drops `report_releases`; the history is rebuilt in
signal from MFN. Once signal and chronicle run the new image:

```bash
kubectl --context orbstack -n chronicle create job nordic-backfill --from=cronjob/nordic-reports \
  --dry-run=client -o json | jq '.spec.template.spec.containers[0].env += [{"name":"NORDIC_BACKFILL","value":"true"}]' \
  | kubectl --context orbstack apply -f -
kubectl --context orbstack -n chronicle create job nordic-calendar-now --from=cronjob/nordic-calendar
```

Check in `backtest`:

```sql
SELECT ticker, event_type, status, count(*), min(occurred_at), max(occurred_at)
FROM special_events
WHERE source = 'mfn'
GROUP BY 1, 2, 3
ORDER BY 1, 2, 3;
```

## Nordic Reports dashboard

Grafana → folder **Nexus** → **Nordic Reports** (`uid nexus-nordic-reports`, template
`infra/charts/nexus/grafana/templates/dashboard-nordic-reports.yaml`). It reads only
`special_events` (datasource `nexus-postgres`): pick a company, see its upcoming dates
(`status = 'scheduled'`, the time is MFN's estimate or "not announced"), its past
reports with the share-price move on the reaction day (`move_pct`, or the `data_status`
text when there is no price), the move per report, and how its releases were timed
against the Stockholm session. The panels ignore the time picker and always show the
company's whole history. A company appears in the dropdown once it has an MFN report
event; the display name comes from `companies.name` (`name` in the registry).

## Live listener (`nordic-live`)

A Deployment (`infra/charts/chronicle/chronicle/templates/deployment-nordic-live.yaml`, 1 replica, `Recreate`) that
connects to MFN's **unofficial** WebSocket `wss://mfn.se/all/s/nordic.json` and **logs** every new item from a company
with an `mfn_slug` the moment it is published. It writes nothing and publishes nothing: the `nordic-reports` poll stays
the only writer and covers anything missed while the socket is down (the stream has no replay, and the server closes it
every 300 s with code 3000, which is a fast reconnect).

```bash
kubectl --context orbstack -n chronicle logs deploy/nordic-live -f
```

- Each new relevant item is one structured log event with `event` = `report_released` (a results release) or
  `company_news`, plus `news_id`, `ticker`, `exchange_mic`, `publish_date`, `received_at`, `latency_ms`, `lang`, `tags`,
  `source`, `report_kind`, `is_correction`, `calendar_update`, `title`, `url` and `attachment_urls` (the PDFs). Classification
  is the same code as the poll (`chronicle_lib::nordic::classify`).
- Every 10 minutes: `nordic-live: N items seen, M relevant`. Registered companies are reloaded every 10 minutes.
- `already ingested by poll`: the item is already in signal's `special_events` (`source = 'mfn'`, `source_id` = the MFN
  news id), checked through `SIGNAL_DATABASE_URL` (read-only). If that lookup fails, the item is logged anyway.
- Reconnects: close code 3000 at once (debug log); anything else backs off 10 ms doubling to 30 s (reset after 60 s up);
  15 minutes without a message or ping drops the connection and reconnects.
- Env: `MFN_WS_URL`, `DATABASE_URL`, `SIGNAL_DATABASE_URL` (optional), `RUST_LOG`, `LOG_FORMAT` (`json` default or `text`).
