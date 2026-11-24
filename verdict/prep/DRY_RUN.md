## This is a dry run

This preparation validates extraction only (`PREP_DRY_RUN=true`). Its kernel is scored on how
exactly it reads figures, never traded, and its decision is not used. Everything above still
applies, with these changes:

* The report in `/work/input.json` has **already been published**, at `report.expected_at`. Prepare
  for it as if it were still the next report: do **not** write `abort.json` because it was
  released. `previous_reports` holds only the reports published before it, and they are your
  input, as in a real run. Do not fetch, search for or read the target report itself; build the
  lookups from the previous reports.
* `input.json` has a `dry_run.metrics` list (`key`, `label`, `unit`). Your kernel must report
  **every one of them**, with exactly that `key`, reading the figure in that unit and scale when
  the release prints it so, and `current` and `prior` as exact decimal strings. Choose the
  lookups for them as for any metric, and list them in `kernel.json`'s `metrics`. You may add
  metrics of your own, for the decision rule.
* A metric you cannot find or verify in a release is left out or has `null` values. In a dry run
  a missing figure is acceptable and a wrong figure is not, so verify before you return one.
