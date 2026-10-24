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
`nordic-reports` run (hourly on weekdays) or start one by hand, then set the
mappings below. The mapping steps never insert.

## Finding the MFN slug

Open the company's page on [mfn.se](https://mfn.se). The slug is the last path
segment of the URL: `https://mfn.se/a/gomspace-group` → `gomspace-group`.

To check the mapping, open `https://mfn.se/all/a/<slug>.json`: the items'
`author.tickers` should contain `<exchange_mic>:<ticker>` (e.g. `FNSE:GOMX`);
otherwise `nordic-reports` logs `mapping may point at the wrong company`.

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
hourly on weekdays 07:00–20:00 plus 22:00, `Europe/Stockholm`) reads every
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
