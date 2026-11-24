# Extraction accuracy fixtures

Past report releases with hand-checked headline metrics. They measure whether a kernel reads the
right numbers (extraction), never whether its decision is good: an agent preparing a kernel today
for a past report can find the outcome on the web, so a dry run says nothing about decision
quality. Scoring and the dry preparation: `docs/validation/analyst-accuracy.md`.

Each fixture is one directory `<country>-<company>-<period>/`:

| File | What it is |
|---|---|
| `release.txt` | The release exactly as the analyst's runtime sees it: the title, a blank line, MFN's `content.text`. Written by `fetch.sh`, never edited by hand. |
| `expected.json` | The company, the release, the company's 8 latest report releases before it (`previous_reports`, the input of a dry preparation, written by `fetch.sh`) and the hand-checked `metrics`. |

Real tickers are allowed here (test fixtures).

## Adding a fixture

```bash
analyst/tests/accuracy/fetch.sh <mfn_slug> <news_id> analyst/tests/accuracy/<dir> '<TICKER>' <MIC> <SE|DK|FI> <industrial|bank|property|biotech>
```

then add 4 to 6 `metrics` to `expected.json` by hand, reading `release.txt`:

```json
{
  "key": "net_sales",
  "label": "Net sales",
  "unit": "MSEK",
  "current": "1234.5",
  "prior": "1101.0",
  "current_line": "Net sales increased to SEK 1,234.5 million (1,101.0)",
  "prior_line": "Net sales increased to SEK 1,234.5 million (1,101.0)"
}
```

* `key`: snake_case, from the vocabulary below when one fits, unique within the fixture.
* `current` / `prior`: this period's figure and the comparison period's figure (usually the one in
  parentheses), as exact decimal strings: `.` as the decimal point, no thousands separator, a
  leading `-` for a negative figure (also one printed as `(12.3)` or with a `−`), in the unit and
  the scale the release prints them in, nothing rounded or converted.
* `unit`: as printed (`SEK bn`, `MSEK`, `DKKm`, `EUR million`, `%`, `SEK`).
* `current_line` / `prior_line`: an exact substring of `release.txt` (line breaks included) that
  shows the figure. The fixture test checks that it is in the text and that the figure's digits are
  in it.
* Pick headline metrics: the ones in the title, the summary bullets or the key-figures table.

Vocabulary: `net_sales`, `revenue`, `organic_growth`, `order_intake`, `order_backlog`, `ebitda`,
`ebita`, `operating_income` (EBIT), `adjusted_operating_income`, `operating_margin`,
`adjusted_operating_margin`, `profit_before_tax`, `net_income`, `eps`, `diluted_eps`,
`operating_cash_flow`, `free_cash_flow`, `net_interest_income`, `net_fee_income`,
`total_income`, `operating_profit` (banks), `loan_losses`, `cost_income_ratio`,
`return_on_equity`, `cet1_ratio`, `rental_income`, `net_operating_income`,
`income_from_property_management`, `property_value`, `loan_to_value`, `cash_position`,
`research_costs`, `net_loss`.

## Coverage the fixture test enforces

At least 24 fixtures; at least 8 each from Swedish, Danish and Finnish companies; per country both
the company's own language and English; the four sectors industrial, bank, property and biotech
among the fixtures of each country; 4 to 6 metrics per fixture.
