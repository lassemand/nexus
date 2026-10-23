# Registering a Nordic company

The Nordic report sources identify companies by their MFN slug, not by ticker,
so each company is mapped once by hand in the `companies` table:

| Column | Meaning |
|---|---|
| `name` | Display name used by the dashboard, free text, e.g. `GomSpace Group AB (GOMX)` |
| `mfn_slug` | Slug of the company page on MFN (`https://mfn.se/a/<slug>`). Must match `^[a-z0-9][a-z0-9-]*$` and be unique |

Both are nullable. (`nasdaq_news_company`, used while releases came from the
Nasdaq news API, was dropped in NEX-166.) The company row (`ticker`, `exchange_mic`) must already exist; these steps never insert.

## Finding the MFN slug

Open the company's page on [mfn.se](https://mfn.se). The slug is the last path
segment of the URL: `https://mfn.se/a/gomspace-group` → `gomspace-group`.

To check the mapping, open `https://mfn.se/all/a/<slug>.json`: the items'
`author.tickers` should contain `<exchange_mic>:<ticker>` (e.g. `FNSE:GOMX`);
otherwise `nordic-reports` logs `mapping may point at the wrong company`.

## SQL

```sql
UPDATE companies
   SET name     = 'GomSpace Group AB (GOMX)',
       mfn_slug = 'gomspace-group'
 WHERE ticker = 'GOMX' AND exchange_mic = 'FNSE';
```

Check that one row was updated (`UPDATE 1`). `UPDATE 0` means the company is not registered yet.

The migration `*_companies_add_nordic_source_mappings.sql` seeds GomSpace the
same way (and did nothing if the row was absent).

## Scheduled and released reports (`report_releases`)

`report_releases` is one table for reports that are **scheduled** and reports
that have been **released**. A row is scheduled while `released_at` and
`mfn_news_id` are NULL (`expected_date`, `expected_time`, `fiscal_period` and
`calendar_label` come from the MFN calendar) and becomes released when the
matching MFN news item arrives. A row needs `released_at` or `expected_date`, and
`mfn_news_id` is NULL exactly when `released_at` is NULL. `expected_date`/
`expected_time` are Europe/Stockholm local and `expected_time` is only an
estimate; `released_at` is the actual UTC instant (MFN's `publish_date`, unchanged).

The `nordic-reports` chronicle binary (CronJob in `infra/charts/chronicle/chronicle/`,
hourly on weekdays 07:00–20:00 plus 22:00, `Europe/Stockholm`) reads every
`FNSE`/`XSTO` company with an `mfn_slug`, pages MFN's JSON news feed
`https://mfn.se/all/a/<slug>.json` newest first (following `next_url`), keeps items
authored by that slug, and records each with `nordic::db::record_release` in one
transaction:

1. `mfn_news_id` already stored → display fields refreshed in place (re-runs are idempotent; classification is decided at insert).
2. Same company, instant and `report_kind` already stored (the Swedish/English twin) → no second row; an English item replaces a non-English one.
3. A real report (not a correction, not a calendar update) → fills the closest **unreleased** row of the same company whose `expected_date` is within ±7 days of the release's Stockholm date (tie: the earlier date), keeping its expected fields. Logged as `matched scheduled report`.
4. Otherwise a new row is inserted with the expected fields NULL.

Incremental runs stop at the first page that has nothing new and whose oldest item
is more than 14 days older than the newest stored release. `--backfill` (or
`NORDIC_BACKFILL=1`) pages to the end; at most 100 pages per company. Companies
without an `mfn_slug` are skipped with an info log. The job exits non-zero if
any company failed.

**One-time backfill** (after deploying NEX-166, and for new companies):

```bash
kubectl --context orbstack -n chronicle create job \
  --from=cronjob/nordic-reports-backfill nordic-reports-backfill-$(date +%s)
```

`nordic-reports-backfill` is a suspended CronJob that runs `nordic-reports --backfill`.

**Classification** (`nordic::classify::classify_item`, from MFN tags + title; the title's issuer prefix `"GomSpace A/S: "` is stripped first). In order:

1. Calendar notice (`financial calendar`, `early publication`, `delayed publication`, `postpone`, `brings forward`, `moves up publication`, `new date for`, `tidigarelägger`, `senarelägger`, `flyttar … rapport`, …) → `calendar_update`, never a report, even if tagged.
2. `Correction:` / `Rättelse:` → `is_correction`, never a report.
3. Year-end → `annual`: tag `sub:report:interim:q4`, or tags `sub:report:annual` + `sub:report:interim`, or a Q4 / year-end / `full year` title that passes the report test below (a `full year` mention is ignored when the title or tags say Q1–Q3 / half-year).
4. Tag `sub:report:annual` only → the published annual report *document* → not a release (`report_kind` NULL, row kept).
5. `interim`: any `sub:report:interim*` tag, or `:regulatory` plus the headline heuristic (report wording, or a quarter mention without business-news wording, and none of the notice words such as `general meeting`, `sustainability`, `invitation`).
6. Otherwise `report_kind` NULL. MFN's tags are incomplete (GomSpace's year-end reports carry only `:regulatory`), so the heuristic is needed.

`fiscal_period` is `YYYY-Qn` (interim) or `YYYY` (year-end) only when the title has exactly one `20xx` year and the quarter is unambiguous; otherwise NULL.

A report row on the same Stockholm date as an *earlier* report row of the same company (a re-send without "Correction") becomes `is_correction` with `report_kind` NULL, and the earlier row's `superseded_by` points at it; the earliest timestamp is the release the market reacted to. Corrections link the same way within 7 days.

Companies with a newly inserted `calendar_update` are logged (`calendar_updated`) and their MFN calendar is refreshed right away.

## Upcoming reports (`nordic-calendar`)

The `nordic-calendar` binary (daily CronJob, 06:30 `Europe/Stockholm`) reads
every company with an `mfn_slug`, fetches the public HTML page
`https://mfn.se/a/<mfn_slug>` and parses its "Kalender" table
(`table.table-calender`). Only the HTML page is used (MFN's `robots.txt`
disallows the JSON/RSS/XML feeds): one request per company per day, at least 2 s
apart, 20 s timeout, `429` honours `Retry-After`. `nordic-reports` also calls the
same `refresh_company` once for a company that has just announced a calendar
change (log: `calendar refreshed after announcement`).

- Rows without `grey-background` are upcoming. Report labels (`Kvartalsrapport`, `Delårsrapport`, `Halvårsrapport`, `Bokslutskommuniké`, `Årsredovisning`, `Interim report`, `Quarterly report`, `Half-year report`, `Year-end report`, `Annual report`) become scheduled `report_releases` rows with `report_kind`, `fiscal_period` (`2026-Q3` / `2026`), `calendar_label` (original text) and `expected_date`. `expected_time` is MFN's **estimate** (the previous report's time) and NULL when MFN shows `-`; the dashboard should label it as estimated.
- Non-report events (AGM, ex-dividend date, capital markets day, …) are ignored; unknown upcoming labels are logged at `warn` with the slug.
- Per company, in one transaction: scheduled rows with `expected_date >= today` (Stockholm) are deleted and re-inserted, so a moved date disappears. Released rows are never deleted or overwritten. A calendar entry that matches an already released report (±7 days, closest wins, released row has no `expected_date` yet) fills that row's expected fields instead of inserting. Scheduled rows whose date has passed without a release stay ("expected, not seen").
- **Parse failures keep the data:** a missing table or no parseable row is logged at `error`, existing rows are kept and the job exits non-zero (Job shows as failed). An empty upcoming list on a parsed table is valid and clears future scheduled rows.
