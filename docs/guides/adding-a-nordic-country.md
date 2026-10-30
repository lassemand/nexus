# Adding a Nordic country

Every fact about a Nordic country lives in one file under `model/src/nordic/`,
and `model::nordic::COUNTRIES` is the one list of supported countries. The rest
of the workspace derives from it: `mic::country`, `mic::currency`, `mic::NORDIC`,
the Nordic Yahoo codes, `exchange::timezone`, the holiday calendars in
`alpha::calendar`, the Saxo ExchangeIds, and the choice of insider source.

**Rule of thumb.** If two countries would write the same function body with
different constants, it is **data** on `Country` or `Venue`. If the code itself
differs, it is a **trait implementation** keyed by the thing that varies: the
insider source (`chronicle::insider_source::InsiderSource`) or the language
(`chronicle::nordic::lang::ReportLanguage`). Holidays and time zones have no
trait: holiday rules are the `ExtraClosure` enum and time zones are names.

The worked example is Norway (Oslo Børs, `XOSL`, which is already listed in
`YAHOO_EXCHANGE_TO_MIC`; NOK, `Europe/Oslo`). **Nothing below has been implemented.**

## Checklist

1. **Country file.** Add `model/src/nordic/no.rs` with `pub const NORWAY: Country`
   (`code: "NO"`, `currency: "NOK"`, `timezone: "Europe/Oslo"`, its `venues`,
   `holidays`, `insider_source`, `languages`). Start the file with a doc comment
   naming the sources (Euronext Oslo trading-hours page, Nager.Date `NO`) and the
   date you checked them. Add `pub mod no;` and the line
   `&no::NORWAY` to `COUNTRIES` in `mod.rs`.
   - MIC constants stay in `model/src/asset.rs` (`mic::*`). Add any new MIC there
     and reference it from the venue. Remove `("OSL", "XOSL")` from
     `YAHOO_EXCHANGE_TO_MIC` and put `"OSL"` in the venue's `yahoo_exchange_codes`:
     a Nordic Yahoo code lives on exactly one venue.
   - `venue.saxo_exchange_id` is a first guess; set `saxo_verified: false` until step 4.
   - Run `cargo test -p model nordic`: the validation tests check unique MICs,
     parseable time zones, 3-letter currencies, `open < close`, a half-day close
     between them and no shared Yahoo codes.
2. **Signal `exchanges` migration.** Add `signal/migrations/<YYYYMMDDHHMMSS>_exchanges_add_norway.sql`
   inserting one row per venue: `mic, country, timezone, open_local, close_local,
   half_day_close_local, currency`, identical to the venue. The signal test
   `exchanges_table_and_model_nordic_agree` fails until the rows and the venues
   match in both directions. Never edit an applied migration.
3. **Holidays.** `HolidayRules` in the country file: Nager.Date names to skip and
   to treat as half days, plus `ExtraClosure`s for closures Nager does not list.
   Then seed the database for the years you need:
   `nexus calendar sync --country NO --year <YEAR>` (one run per year, covering
   at least the report history and the current year; outside the seeded years a
   reaction is `calendar_unknown`). Check the result against the exchange's
   published calendar.
4. **Saxo id.** Look the venue up with `GET /ref/v1/exchanges` and
   `GET /ref/v1/exchanges/{ExchangeId}` (the `Mic` field), correct
   `saxo_exchange_id`, and set `saxo_verified: true`. A deployment can override a
   wrong id without a rebuild with `SAXO_EXCHANGE_IDS=MIC=Id,...`.
5. **MFN coverage and language.** Check that the country's issuers are on MFN
   (`https://mfn.se/search/companies?query=<name>` lists `<MIC>:<TICKER>` for each
   mapped company). Take the languages their headlines use into
   `Country::languages`. If a language is new (Norwegian: `Lang::Nb`), add the
   variant to `Lang`, a file `chronicle/src/nordic/lang/nb.rs` implementing
   `ReportLanguage` (report words, non-report words, calendar phrases, correction
   markers, month names, quarter phrases; override only what differs) and a line in
   `lang::report_language` and `lang::ALL`. Add real Norwegian headlines to
   `chronicle/tests/fixtures/classify_titles.txt` and regenerate the classify
   snapshot deliberately (`UPDATE_SNAPSHOTS=1 cargo test -p chronicle --test classify_snapshot`),
   reviewing the diff.
6. **Insider source.** Pick `Country::insider_source`. Reuse an existing kind
   (`MarReleases` works for any country whose issuers publish MAR Art. 19 releases on
   MFN; `FiRegister` is Sweden's register; `None` means no insider data). A
   register of its own is a new `InsiderSourceKind` variant plus an `InsiderSource`
   implementation in `chronicle/src/insider_source/`, wired in `insider_source(kind)`.
7. **Snapshots.** Add the new MICs to `alpha/tests/nordic_snapshot.rs`, save the
   Nager responses under `alpha/tests/fixtures/nager/`, and regenerate
   `alpha/tests/snapshots/` deliberately.
8. **Docs.** Update `docs/guides/nordic-companies.md` if behaviour for companies
   changes. Do not name individual companies (see CLAUDE.md, "Companies are
   runtime data").

## What you do not edit

Anything that already reads `model::nordic`: `mic::country`/`currency`/`NORDIC`,
`exchange::timezone`, `alpha::calendar`, `alpha::saxo::exchange_ids`, `cli`'s
Nordic checks, and the chronicle jobs' MIC lists. If one of them needs a change
for a new country, the fact belongs on `Country` or `Venue` instead.
