# Registering a Nordic company

The Nordic report sources identify companies by their own keys, not by ticker,
so each company is mapped once by hand in the `companies` table:

| Column | Meaning |
|---|---|
| `name` | Display name used by the dashboard, free text, e.g. `GomSpace Group AB (GOMX)` |
| `mfn_slug` | Slug of the company page on MFN (`https://mfn.se/a/<slug>`). Must match `^[a-z0-9][a-z0-9-]*$` and be unique |
| `nasdaq_news_company` | Issuer name exactly as the Nasdaq Nordic news feed spells it (byte for byte). Unique |

All three are nullable. The company row (`ticker`, `exchange_mic`) must already exist; these steps never insert.

## Finding the MFN slug

Open the company's page on [mfn.se](https://mfn.se). The slug is the last path
segment of the URL: `https://mfn.se/a/gomspace-group` → `gomspace-group`.

## Finding the Nasdaq issuer name

Query the news API and copy the `company` field of any item for that company:
`https://api.news.eu.nasdaq.com/news/query.action` (e.g. `"company": "GomSpace Group AB"`).
Copy it verbatim, including case and punctuation.

## SQL

```sql
UPDATE companies
   SET name                = 'GomSpace Group AB (GOMX)',
       mfn_slug            = 'gomspace-group',
       nasdaq_news_company = 'GomSpace Group AB'
 WHERE ticker = 'GOMX' AND exchange_mic = 'FNSE';
```

Check that one row was updated (`UPDATE 1`). `UPDATE 0` means the company is not registered yet.

The migration `*_companies_add_nordic_source_mappings.sql` runs this same
update for GomSpace, and does nothing if the row is absent.

## Scheduled and released reports (`report_releases`)

`report_releases` is one table for reports that are **scheduled** and reports
that have been **released**. A row is scheduled while `released_at` and
`disclosure_id` are NULL (the `expected_date`, `expected_time`, `fiscal_period`
and `calendar_label` columns come from the MFN calendar, NEX-161) and becomes
released when the matching Nasdaq disclosure arrives. Constraints: a row needs
`released_at` or `expected_date`, and `disclosure_id` is NULL exactly when
`released_at` is NULL. `expected_date`/`expected_time` are Europe/Stockholm local
and `expected_time` is only an estimate; `released_at` is the actual UTC instant.

The `nordic-reports` chronicle binary (CronJob in `infra/charts/chronicle/chronicle/`,
hourly on weekdays 07:00–20:00 plus 22:00, `Europe/Stockholm`) reads every
`FNSE`/`XSTO` company with a `nasdaq_news_company` and records the exchange's
disclosures with `nordic::db::record_release`, per item, in one transaction:

1. `disclosure_id` already stored → that row is updated in place (re-runs are idempotent).
2. A real report (not a correction, not a calendar update) → fills the closest **unreleased** row of the same company whose `expected_date` is within ±7 days of the release's Stockholm date (tie: the earlier date), keeping its expected fields. Logged as `matched scheduled report`.
3. Otherwise a new row is inserted with the expected fields NULL.

Corrections, non-reports and calendar updates never match a scheduled row.
Unmapped companies are skipped with an info log.

- `released_at` is a UTC instant (the request uses `timeZone=UTC`; `company=` matches the issuer name exactly).
- `report_kind` is `interim` or `annual` for periodic reports and `NULL` for everything else. Rule: category in `Interim report (Q1 and Q3)`, `Half-year financial report`, `Annual financial report`, `Interim information`, and then `Annual financial report` always counts; otherwise the headline needs report wording (`report`, `interim`, `half-year`, `year-end`, `annual`, `rapport`, …) or a quarter mention (`Q1`–`Q4`, `quarter`, `kvartal`) without business-news wording (`order intake`, `guidance`, `contract`, `agreement`, `secures`, `signs`, `market update`).
- `is_correction` marks `Correction:` / `Rättelse:` headlines (never `report_kind`); the original report's `superseded_by` points at the correction (within 7 days).
- `calendar_update` marks financial-calendar announcements; companies with newly inserted ones are logged as `calendar_updated`.
