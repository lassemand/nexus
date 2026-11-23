# CI task selection with moon

CI runs only the checks a change can affect. The selection is made by
[moon](https://moonrepo.dev) (moonrepo), a task runner with affected
detection; the version is pinned in `.prototools` (`moon = "..."`, install with
`proto install moon` or the release binary of that version). Plain `cargo`
still works for everything; moon only decides which `cargo` (and shell)
commands to run.

## Projects and tasks

| Project | Where it comes from | Tasks |
|---|---|---|
| every Cargo package at `*/`, `verdict/*/`, `verdict/kernels/*/` | its `Cargo.toml` (`.moon/workspace.yml` globs) | `lint` (`cargo clippy -p <crate> --all-targets --all-features -- -D warnings`), `test` (`cargo test -p <crate>`), from `.moon/tasks/rust.yml` |
| `telemetry` | as above | also `test-kafka`, `test-collector-down` |
| `verdict/kernels/sample` | as above | also `wasm`: clippy and release build of `verdict-sdk`/`verdict-sample` for `wasm32-unknown-unknown`; inputs `verdict/sdk/**`, `verdict/kernels/**` |
| `verdict/bench` (own Cargo workspace) | as above | `fmt`, `lint`, `lint-wasm`, `build`, `build-wasm`, `test` |
| `verdict/template` (own Cargo workspace) | as above | `fmt`, `lint`, `test`, `build-wasm` |
| `root` | `moon.yml` at the root | `fmt` (`cargo fmt --all -- --check`), input: every file, so it runs on every change |
| `verdict/prep` | listed in `.moon/workspace.yml` | `syntax` (`bash -n`), `test` (`test_entrypoint.sh`; also reads `verdict/sdk`, `verdict/template`, `infra/agents/analyst.md`) |
| `conductor/deploy` | listed | `shellcheck`, `test` (`entrypoint-test.sh`), `isolation` (`review-isolation-test.sh`; needs the switch helper installed with sudo and setcap first, so `moon ci` leaves it out and a separate job runs it) |
| `infra/charts/nexus/prometheus` | listed | `rules`: `promtool check rules` and `promtool test rules` in `prom/prometheus:v3.15.0`; input `rules/**` |

Project ids are the folder paths (`verdict/sdk`, `conductor/deploy`); the Rust
toolchain plugin adds the package name as an alias (`$projectAlias`, used in
`cargo -p`). `moon query projects` and `moon task <id>:<task>` show the result.

All Rust tasks use one target directory: `CARGO_TARGET_DIR` from the
environment, else `<repo>/target`. moon never caches or restores `target/`
(no task has outputs, and task caching is off), and Postgres settings
(`DATABASE_URL`, `TEST_DATABASE_URL`) come from the environment.

## How selection works

1. **Files to tasks.** A task is affected when a touched file matches its
   inputs. A Rust task's inputs are its project folder plus the run-everything
   files below. A file in a nested project (`conductor/deploy/`) is excluded
   from the outer crate's inputs except for the files its tests read.
2. **Crates to dependents.** The Rust plugin reads each project's
   `Cargo.toml` and turns every `path = ...` dependency (normal, dev, build,
   per target, `workspace = true`) into a project dependency. Every task of a
   Rust project depends on the `lint` of the projects it depends on
   (`implicitDeps: ['^:lint']`), and `--include-relations --downstream deep`
   adds every task above an affected `lint`, transitively. So a change to
   `model` selects `verdict/bench`, which reaches `model` only through
   `analyst`. A change seen only by tests (a migration read by another crate's
   tests) affects those tests, not the crates above them.
3. **Nothing upstream.** `--upstream none` keeps those deps from running: they
   express "depends on", not "must run first".

The command CI runs is `scripts/ci/moon-ci.sh`:

```bash
git diff --name-only --no-renames $(git merge-base origin/main HEAD) HEAD |
  moon ci --stdin --include-relations --downstream deep --upstream none
```

`moon ci` runs every affected task with `runInCI` (all but
`conductor/deploy:isolation`). `MOON_BASE`/`MOON_HEAD` change the range; extra
arguments go to `moon ci` (targets such as `:test`, `--job`/`--job-total`).

**When in doubt, everything.** The script runs `moon ci --force` (every task)
when the base or head is missing, they have no merge base (a shallow clone or
a missing fetch), the diff is empty (a push to main), or moon fails before it
starts a task. It never runs nothing.

Do not decide what runs from `moon query tasks --affected`: it can disagree
with what `moon run --affected` executes in CI (moon issue 2706). The tests
below read what `moon ci` actually ran from `.moon/cache/ciReport.json`.

Locally:

```bash
moon run :test :lint --affected --include-relations --downstream deep --upstream none                    # uncommitted changes
moon run :test :lint --affected --base origin/main --include-relations --downstream deep --upstream none  # the branch
MOON_BASE=origin/main scripts/ci/moon-ci.sh                                                              # as CI
```

Without `--include-relations --downstream deep` moon only runs the changed
crate's own tasks, not the crates above it.

## Run-everything files

These are inputs of every Rust task (`fileGroups.globals` in
`.moon/tasks/all.yml`), so a change to any of them runs every crate, both
other workspaces, the WASM task and the prep test:

- `Cargo.toml` (root), `Cargo.lock`, `rust-toolchain.toml`
- `.cargo/**`
- `.moon/**`
- `.github/workflows/ci.yml`

A `Cargo.lock` change is not narrowed to the crates whose dependencies changed.

## Adding a crate

Add it to the root Cargo workspace. Nothing else: a folder at `*/` or
`verdict/kernels/*/` with a `Cargo.toml` is a project, it inherits `lint` and
`test`, and its `path` dependencies become project dependencies. If it lives
elsewhere, add a glob to `projects.globs` in `.moon/workspace.yml`;
`scripts/ci/check-moon-graph.py` fails until every workspace member is a
project.

## Declaring a cross-crate input

A crate that reads a file from another project's folder at build or test time
has to declare it, or a change there will not run it. Add the path,
workspace-relative with a leading `/`, to that crate's `moon.yml`:

```yaml
tasks:
  test:
    inputs:
      - '/signal/migrations/**/*'
  lint:            # only if the file is compiled in (include_str!, include_bytes!, #[path])
    inputs:
      - '/signal/migrations/**/*'
```

Today: `analyst` and `chronicle` tests read `signal/migrations/`; `signal`
compiles in `chronicle/tests/fixtures/mfn/e2e/*.pb`; `verdict-sdk`'s
`tests/wasm_abi.rs` builds `verdict/kernels/sample`; `conductor`'s tests read
`conductor/deploy/reviewer-settings.json` and `conductor/deploy/review-bin/`.

`scripts/ci/check-cross-crate-inputs.py` scans every Rust project for
`include_str!`/`include_bytes!` and `#[path]` (relative to the file),
`sqlx::migrate!("...")` and `CARGO_MANIFEST_DIR` joined with a path (relative
to the package), and fails when one resolves outside the project and is not
covered by its inputs. A read that names a wider folder than it uses (the
workspace root, in `wasm_abi.rs`) is narrowed in the script's `NARROWED` table.
Reads it cannot see (a path built at run time, a `cargo build -p` of another
crate) still have to be declared by hand.

## Checks

| Script | Fails when |
|---|---|
| `scripts/ci/check-moon-graph.py` | moon's project dependencies differ from the `path` dependencies in `cargo metadata` (root workspace, `verdict/bench`, `verdict/template`), or a workspace member is not a project |
| `scripts/ci/check-cross-crate-inputs.py` | a file read from another project's folder is not a declared input |
| `scripts/ci/test-affected.sh` | `moon ci`, through `moon-ci.sh`, runs other tasks than expected for a fixed list of touched files (given on stdin, no commits), or does not run everything when the base is unknown, the diff is empty or the clone is shallow |

`test-affected.sh` works in a scratch copy where the moon configuration,
`Cargo.toml`s and `Cargo.lock` are real, every other file is empty, and
`cargo`, `docker` and `shellcheck` are stubs, so it builds nothing (about 10 s).
Update its expected sets when a project or a dependency changes on purpose.
