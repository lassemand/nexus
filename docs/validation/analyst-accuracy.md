# Analyst: extraction accuracy of dry-run kernels

NEX-259. Before anyone reads meaning into the analyst's simulated P&L, this checks that a prepared
kernel reads the **right numbers** from a release. Only extraction is scored. Decision quality
cannot be measured on past reports: an agent preparing a kernel today for a past report can find
the outcome on the web. That look-ahead is acceptable here because no decision is scored. Quality
is measured going forward, in the pilot (`analyst-pilot.md`).

## Status: fixtures and tooling ready, the dry preparations not yet run

| Step | State |
|---|---|
| 24 hand-checked fixtures (`analyst/tests/accuracy/`) | **Done**, checked in CI (`analyst/tests/accuracy_fixtures.rs`) |
| Dry-run mode of the preparation Job (`PREP_DRY_RUN=true`) | **Done**, tested in `verdict/prep/test_entrypoint.sh` |
| Scoring through the analyst's runtime (`analyst-accuracy score`) | **Done**, tested in `analyst/tests/accuracy_fixtures.rs` |
| One dry preparation per fixture, then the score | **Not run.** It needs the Job launcher and the kernel acceptance endpoint in `analyst` (the service still runs `NoopLauncher`, and nothing serves `POST :8081/kernels`). Until those exist, the Jobs can be started by hand with the commands below |
| Result and miss list (below) | Waiting for the run |

## The fixtures

Each fixture is one past report release. `release.txt` is exactly what the runtime sees (the
title, a blank line, MFN's `content.text`), written by `analyst/tests/accuracy/fetch.sh`, never by
hand. `expected.json` holds 4 to 6 hand-checked headline metrics, each with its current and prior
figure, its unit and the exact line of the release that shows each figure, plus the company's 8
latest report releases before it (the dry run's input). The format is in
`analyst/tests/accuracy/README.md`.

There are 117 metrics, so 234 hand-checked values. At 95 %, at least 223 must be exact and none
wrong.

| Fixture | Company | Venue:ticker | Sector | Lang | Kind | Published | Metrics |
|---|---|---|---|---|---|---|---|
| `dk-chemometec-2026q3` | ChemoMetec | XCSE:CHEMM | biotech | da | interim | 2026-11-05 | 4 |
| `dk-dfds-2026q3` | DFDS | XCSE:DFDS | industrial | da | interim | 2026-11-11 | 4 |
| `dk-djurslands-bank-2026q3` | Djurslands Bank | XCSE:DJUR | bank | da | interim | 2026-11-06 | 5 |
| `dk-fast-ejendom-danmark-2026q3` | Fast Ejendom Danmark | XCSE:FED | property | da | interim | 2026-10-22 | 5 |
| `dk-genmab-2026q3` | Genmab | XCSE:GMAB | biotech | en | interim | 2026-11-05 | 4 |
| `dk-nkt-2026q3` | NKT | XCSE:NKT | industrial | en | interim | 2026-11-19 | 5 |
| `dk-park-street-2026q2` | Park Street | XCSE:PARKST A | property | en | interim | 2026-08-25 | 4 |
| `dk-ringkjobing-landbobank-2026q3` | Ringkjøbing Landbobank | XCSE:RILBA | bank | da | interim | 2026-10-21 | 5 |
| `fi-alandsbanken-2026q3` | Ålandsbanken | XHEL:ALBBV | bank | en | interim | 2026-10-23 | 5 |
| `fi-citycon-2026q2` | Citycon | XHEL:CTY1S | property | en | interim | 2026-08-12 | 5 |
| `fi-kone-2026q3` | Kone | XHEL:KNEBV | industrial | fi | interim | 2026-10-28 | 5 |
| `fi-konecranes-2026q2` | Konecranes | XHEL:KCR | industrial | en | interim | 2026-07-24 | 5 |
| `fi-lumo-2026q3` | Lumo Kodit | XHEL:LUMO | property | fi | interim | 2026-11-05 | 5 |
| `fi-omasp-2026q2` | Oma Säästöpankki | XHEL:OMASP | bank | fi | interim | 2026-08-13 | 5 |
| `fi-orion-2026q3` | Orion | XHEL:ORNBV | biotech | en | interim | 2026-10-28 | 4 |
| `fi-revenio-2026q3` | Revenio Group | XHEL:REG1V | biotech | fi | interim | 2026-11-12 | 5 |
| `se-camurus-2026q3` | Camurus | XSTO:CAMX | biotech | en | interim | 2026-11-05 | 5 |
| `se-castellum-2026q2` | Castellum | XSTO:CAST | property | sv | interim | 2026-07-15 | 6 |
| `se-getinge-2026q3` | Getinge | XSTO:GETI B | biotech | en | interim | 2026-10-21 | 5 |
| `se-handelsbanken-2026q3` | Handelsbanken | XSTO:SHB A | bank | sv | interim | 2026-10-21 | 5 |
| `se-nordnet-2026q3` | Nordnet | XSTO:SAVE | bank | en | interim | 2026-10-22 | 4 |
| `se-skf-2026q3` | SKF | XSTO:SKF B | industrial | sv | interim | 2026-10-21 | 5 |
| `se-volvo-2026q3` | Volvo | XSTO:VOLV B | industrial | en | interim | 2026-10-23 | 6 |
| `se-wallenstam-2025fy` | Wallenstam | XSTO:WALL B | property | sv | annual | 2026-02-05 | 6 |

By country: 8 Swedish (4 sv, 4 en), 8 Danish (5 da, 3 en), 8 Finnish (4 fi, 4 en). Each country has
at least one industrial, bank, property and biotech issuer. "Biotech" includes pharma and medtech
(Getinge, Orion, Revenio, ChemoMetec).

### What makes them hard

They are deliberately not all the easy "figure (prior)" layout:

* **Key-figure tables** with box-drawing columns (DFDS, Djurslands Bank, NKT, ChemoMetec, Citycon,
  Ringkjøbing Landbobank). Ringkjøbing prints four columns (Q3, Q3 last year, nine months, nine
  months last year), and the fixture uses the nine-month pair. Ålandsbanken's table has the
  previous quarter between the current and the prior figure.
* **Quarter and year-to-date side by side.** Each label says which period is meant: the quarter
  for most, nine months for the Danish banks and Genmab, half year for Park Street and Oma
  Säästöpankki.
* **A prior that is not last year.** Handelsbanken's headline compares with the previous quarter
  (Q2 2026), and its labels say so.
* **Number formats.** `1.234,5` (Danish), `1 234,5` with a normal or non-breaking space (Swedish,
  Finnish), `1,234.5` (English), negative figures as `-20.2`, `-0,27` and `- 40` (DFDS), figures
  broken across lines (Volvo, Genmab, Castellum).
* **Scale in the wording.** For example Volvo's net sales in SEK billion next to operating income
  in SEK M, and Camurus' cash in BNSEK next to everything else in MSEK. The figure is scored in
  the scale the release prints it in.

## How a value is scored

`analyst-accuracy score` loads each fixture's kernel with the analyst's own `KernelRuntime` (the
same limits as in production), runs it on `release.txt` in the release's language, and compares
every hand-checked `current` and `prior` with the kernel's metric of the same `key`:

* **exact**: the kernel's figure is the same decimal (`1234.50` equals `1234.5`);
* **missing**: the metric is absent, the figure is `null`, or there is no verdict (no kernel, a
  load or run error). Missing is acceptable;
* **wrong**: any other figure, including one that is not a decimal (`12,3`) and a second report of
  the same key with a different figure. Wrong is never acceptable.

**Pass:** at least 95 % of the values exact and none wrong. Nothing in the tool touches a
database, Kafka or a quote, so a dry run can never create a decision or a trade.

## Running it

1. Write the dry-run inputs (one `PREP_INPUT` per fixture: the fixture's report as the next report,
   `prep_started_at` 12 h before its publish time, only the earlier reports, and in
   `dry_run.metrics` the keys, labels and units the kernel must report, never a figure or the
   release itself):

   ```bash
   cargo run -p analyst --bin analyst-accuracy -- inputs --out /tmp/accuracy/inputs
   ```

2. Run the normal preparation Job once per fixture, with `PREP_DRY_RUN=true` and
   `PREP_INPUT=$(cat /tmp/accuracy/inputs/<fixture>.json)`. In dry-run mode the entrypoint adds
   `verdict/prep/DRY_RUN.md` to the prompt (the report has been published, so do not abort as
   `already_released`, do not read it, and report every `dry_run.metrics` key), marks the upload
   `"dry_run": true`, and also writes the upload body to `PREP_OUTPUT` when it is set. With
   `PREP_OUTPUT` set, `PREP_UPLOAD_URL` may be left out. Until the launcher exists, a hand-started
   Job (or `docker run` of the prep image) with `PREP_OUTPUT=/work/out/<fixture>.json` on a
   mounted volume does this.

3. Take the WASM out of each upload body and score:

   ```bash
   for f in /tmp/accuracy/out/*.json; do
     jq -r .wasm "$f" | base64 -d > /tmp/accuracy/kernels/"$(basename "$f" .json)".wasm
   done
   cargo run -p analyst --bin analyst-accuracy -- score --kernels /tmp/accuracy/kernels \
     --markdown /tmp/accuracy/result.md --json /tmp/accuracy/scores.json
   ```

   It exits 0 on a pass and 1 on a fail. `result.md` holds the totals, one row per fixture and
   every miss. Paste it below, and add the cause of each miss (read from the kernel's `kernel.json`
   and source in the upload body).

## Result

Not run yet (see Status).

## Misses and causes

Not run yet.
