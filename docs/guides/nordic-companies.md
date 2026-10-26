# Registering a Nordic company

The Nordic report sources identify companies by their MFN slug, not by ticker,
so each company is mapped once by hand in the `companies` table:

| Column | Meaning |
|---|---|
| `name` | Display name used by the dashboard, free text, e.g. `GomSpace Group AB (GOMX)` |
| `mfn_slug` | Slug of the company page on MFN (`https://mfn.se/a/<slug>`). Must match `^[a-z0-9][a-z0-9-]*$` and be unique |
| `fi_issuer` | Issuer name exactly as Finansinspektionen's insider register spells it. Used by `pdmr` for PDMR (insider) transactions. Unique |

All three are nullable. (`nasdaq_news_company`, used while releases came from the
Nasdaq news API, was dropped in NEX-166.) The company row itself (`ticker`,
`exchange_mic`, `currency`) is created from the registration: `nexus register
GOMX.ST` publishes to `company.tickers`, and `pdmr`, `nordic-reports` and
`nordic-calendar` copy new registrations into `companies` when they start
(`chronicle_lib::registry`). After registering, wait for the next
`nordic-reports` run (daily at 22:00) or start one by hand, then set the
mappings below. The mapping steps never insert.

## Finding the MFN slug

Open the company's page on [mfn.se](https://mfn.se). The slug is the last path
segment of the URL: `https://mfn.se/a/gomspace-group` → `gomspace-group`.

To check the mapping, open `https://mfn.se/all/a/<slug>.json`: the items'
`author.tickers` should contain `<exchange_mic>:<ticker>` (e.g. `FNSE:GOMX`);
otherwise `nordic-reports` logs `mapping may point at the wrong company`.

## Danish companies

Danish listings (Nasdaq Copenhagen `XCSE`, First North Denmark `FNDK`) are
registered the same way:

```bash
nexus register NOVO-B.CO
```

The ISIN is the one identifier MFN, Saxo and Yahoo share whatever they call the
ticker. It is not given at registration: `nordic-reports` reads it from MFN's
`author.isins` the first time it fetches a company's feed (so after `mfn_slug`
is set) and stores it in `companies.isin` when that is empty. It is never
overwritten, and when MFN lists several valid ISINs (several share classes) none
is stored; set it by hand then.

- Yahoo reports Copenhagen as `CPH` for both markets, so a First North Denmark
  company needs `--mic FNDK`. Likewise `--mic XSTO` for a Stockholm main-market
  share (`STO` alone gives `FNSE`). The MIC must be in the same country as
  Yahoo's exchange; `--mic FNDK` on a `.ST` symbol is an error.
- MFN slug: `https://mfn.se/a/novo-nordisk` → `novo-nordisk`.
- MFN spells the ticker `XCSE:NOVO B` (space) while the registry has `NOVO-B`
  (hyphen). This is expected: the registry keeps the Yahoo spelling, and other
  sources map through `mfn_slug` and `isin`.
- Check the ISIN at `https://mfn.se/all/a/<slug>.json` → `author.isins`.
- Set the mappings by hand for later companies:

```sql
UPDATE companies
   SET name     = 'Novo Nordisk A/S (NOVO B)',
       mfn_slug = 'novo-nordisk',
       isin     = 'DK0062498333'
 WHERE ticker = 'NOVO-B' AND exchange_mic = 'XCSE';
```

`UPDATE 0` means the company is not registered yet. The migration
`*_companies_isin.sql` seeds Novo Nordisk and GomSpace (`SE0008348304`) this
way and does nothing if the row is absent.

**Danish insider trades are not covered**: there is no FI-style register for
them, deliberately deferred.

## Finding the FI issuer name

Query FI's issuer autocomplete with part of the company name (not the ticker)
and copy the current name verbatim:
`https://marknadssok.fi.se/Publiceringsklient/sv-SE/AutoComplete/H%C3%A4mtaAutoCompleteListaFull?sokfunktion=Insyn&falt=Utgivare&sokterm=GomSpace`
returns `["GomSpace AB","GomSpace Group AB"]`. Without `fi_issuer`, pdmr searches
with the ticker, which only works when the ticker is part of the issuer name.

## SQL

```sql
UPDATE companies
   SET name      = 'GomSpace Group AB (GOMX)',
       mfn_slug  = 'gomspace-group',
       fi_issuer = 'GomSpace Group AB'
 WHERE ticker = 'GOMX' AND exchange_mic = 'FNSE';
```

Check that one row was updated (`UPDATE 1`). `UPDATE 0` means the company is not registered yet.

The migration `*_companies_add_nordic_source_mappings.sql` seeds GomSpace the
same way (and did nothing if the row was absent).

## Trading holidays (Sweden, Denmark)

`trading_holidays` (signal DB) is seeded per **country**, not per MIC:
`FNSE`/`XSTO` use `SE`, `XCSE`/`FNDK` use `DK` (`alpha::calendar::EXCHANGE_TO_COUNTRY`).
Session hours per MIC are in the `exchanges` table (Copenhagen: 09:00-17:00
`Europe/Copenhagen`, no half days; Stockholm: 09:00-17:30, 13:00 half-day close).

Denmark's calendar adds closures Nager.Date may not list: the Friday after
Ascension, 5 June, 24 and 31 December (fully closed), and Great Prayer Day
(Store Bededag) for years up to and including 2023.

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
`FNSE`/`XSTO` company with an `mfn_slug`, pages MFN's JSON news feed
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
