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

For a listing on `FNSE`, `XSTO`, `XCSE` or `FNDK`, `nexus register`:

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
`FNSE`/`XSTO` use `SE`, `XCSE`/`FNDK` use `DK`, `XHEL`/`FNFI` use `FI` (`alpha::calendar::EXCHANGE_TO_COUNTRY`).
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
6. A calendar snapshot (ignored if older than one already applied) replaces the company's scheduled rows with `expected_date >= today`; an entry matching an occurred report without expected fields (±7 days) fills them instead. Past scheduled rows stay ("expected, not seen"). An empty snapshot clears the future ones.

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
