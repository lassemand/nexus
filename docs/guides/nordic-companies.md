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
