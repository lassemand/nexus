#!/usr/bin/env python3
"""Checks that every file a crate reads from another project is a declared moon input.

moon decides what a change affects from each task's inputs. A Rust task's
inputs are its own project folder plus the run-everything files; a crate that
reads files from another project's folder at build or test time has to declare
them in its moon.yml (docs/guides/ci.md). This scans the tracked `*.rs` files
(build.rs included) of every Rust project for:

  * `include_str!` / `include_bytes!` and `#[path = ...]`, relative to the file;
  * `sqlx::migrate!("...")` and `#[sqlx::test(migrations = "...")]`, relative
    to the package (CARGO_MANIFEST_DIR);
  * `env!("CARGO_MANIFEST_DIR")` followed, in the same statement, by
    `.join("...")` or `concat!` literals;

resolves each path, and, when it leaves the project's folder, checks that every
tracked file under it is an input of the project's `test` task, and also of its
`lint` task for the compiled-in reads (`include_*!`, `#[path]`). Fails on any
path that is not covered.

Usage: scripts/ci/check-cross-crate-inputs.py   (needs moon on PATH)
"""

import json
import os
import re
import subprocess
import sys

# A read that names a broader folder than it uses: (file, resolved path) -> the
# path it really reads, which is then the one that has to be covered.
NARROWED = {
    # Runs `cargo build -p verdict-sample` from the workspace root.
    ("verdict/sdk/tests/wasm_abi.rs", ""): "verdict/kernels/sample",
}

INCLUDE = re.compile(r'\binclude_(?:str|bytes)!\s*\(\s*"([^"]+)"', re.S)
PATH_ATTR = re.compile(r'#\[\s*path\s*=\s*"([^"]+)"\s*\]')
MIGRATE = re.compile(r'\bmigrate!\s*\(\s*"([^"]+)"', re.S)
SQLX_TEST = re.compile(r'#\[\s*(?:sqlx::)?test\s*\(([^\]]*)\)\s*\]', re.S)
MIGRATIONS_ARG = re.compile(r'\bmigrations\s*=\s*"([^"]+)"')
MANIFEST_DIR = re.compile(r'env!\s*\(\s*"CARGO_MANIFEST_DIR"\s*\)')
JOIN_OR_CONCAT = re.compile(r'\.join\(\s*"([^"]*)"\s*\)|^\s*,\s*"([^"]*)"|\)\s*,\s*"([^"]*)"')


def glob_regex(glob):
    out, i = "", 0
    while i < len(glob):
        if glob.startswith("**/", i):
            out += "(?:.*/)?"
            i += 3
        elif glob.startswith("/**", i) and i + 3 == len(glob):
            out += "(?:/.*)?"
            i += 3
        elif glob.startswith("**", i):
            out += ".*"
            i += 2
        elif glob[i] == "*":
            out += "[^/]*"
            i += 1
        elif glob[i] == "?":
            out += "[^/]"
            i += 1
        elif glob[i] == "{":
            end = glob.index("}", i)
            out += "(?:" + "|".join(re.escape(p) for p in glob[i + 1 : end].split(",")) + ")"
            i = end + 1
        else:
            out += re.escape(glob[i])
            i += 1
    return re.compile(out + r"\Z")


class Inputs:
    def __init__(self, task):
        self.files = set(task.get("inputFiles") or {})
        self.include, self.exclude = [], []
        for g in task.get("inputGlobs") or {}:
            (self.exclude if g.startswith("!") else self.include).append(glob_regex(g.lstrip("!")))

    def covers(self, path):
        if path in self.files:
            return True
        return any(r.match(path) for r in self.include) and not any(r.match(path) for r in self.exclude)


def main():
    root = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"], check=True, capture_output=True, text=True
    ).stdout.strip()
    os.chdir(root)
    moon = os.environ.get("MOON", "moon")

    projects = json.loads(
        subprocess.run([moon, "query", "projects"], check=True, capture_output=True, text=True).stdout
    )["projects"]
    rust = {
        p["source"]: p
        for p in projects
        if "rust" in (p.get("toolchains") or []) and p["source"] != "."
    }
    tracked = subprocess.run(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.split("\0")
    tracked = [f for f in tracked if f and os.path.exists(f)]

    def project_of(path):
        best = None
        for src in rust:
            if path.startswith(src + "/") and (best is None or len(src) > len(best)):
                best = src
        return best

    def package_dir(path):
        d = os.path.dirname(path)
        while d and not os.path.exists(os.path.join(d, "Cargo.toml")):
            d = os.path.dirname(d)
        return d

    def norm(path):
        p = os.path.normpath(path).replace(os.sep, "/")
        return "" if p == "." else p

    def reads(path, text):
        here = os.path.dirname(path)
        pkg = package_dir(path)
        for m in INCLUDE.finditer(text):
            yield m.start(), norm(os.path.join(here, m.group(1))), True
        for m in PATH_ATTR.finditer(text):
            yield m.start(), norm(os.path.join(here, m.group(1))), True
        for m in MIGRATE.finditer(text):
            yield m.start(), norm(os.path.join(pkg, m.group(1))), False
        for m in SQLX_TEST.finditer(text):
            for arg in MIGRATIONS_ARG.finditer(m.group(1)):
                yield m.start(), norm(os.path.join(pkg, arg.group(1))), False
        for m in MANIFEST_DIR.finditer(text):
            statement = text[m.end() : text.find(";", m.end()) % (len(text) + 1)]
            resolved = pkg
            for j in JOIN_OR_CONCAT.finditer(statement):
                literal = next(g for g in j.groups() if g is not None)
                resolved = os.path.join(resolved, literal.lstrip("/"))
            yield m.start(), norm(resolved), False

    errors, found = [], 0
    for path in sorted(tracked):
        if not path.endswith(".rs"):
            continue
        src = project_of(path)
        if src is None:
            continue
        text = open(path, encoding="utf-8", errors="replace").read()
        tasks = rust[src].get("tasks") or {}
        for offset, target, compiled in reads(path, text):
            if target == src or target.startswith(src + "/"):
                continue
            found += 1
            line = text.count("\n", 0, offset) + 1
            target = NARROWED.get((path, target), target)
            files = [f for f in tracked if f == target or f.startswith(target + "/" if target else "")]
            if not files:
                errors.append(f"{path}:{line}: reads {target or '.'}, which has no tracked files")
                continue
            for name in (["lint", "test"] if compiled else ["test"]):
                if name not in tasks:
                    errors.append(f"{path}:{line}: reads {target or '.'}, but {rust[src]['id']} has no {name} task")
                    continue
                inputs = Inputs(tasks[name])
                missing = [f for f in files if not inputs.covers(f)]
                if missing:
                    hint = f"/{target}" if os.path.isfile(target) else f"/{target}/**/*" if target else "/**/*"
                    errors.append(
                        f"{path}:{line}: reads {target or '.'}, not an input of {rust[src]['id']}:{name}"
                        f" ({len(missing)} files, e.g. {missing[0]}); declare '{hint}' in {src}/moon.yml"
                    )

    if errors:
        print("cross-crate reads that are not declared moon inputs:", file=sys.stderr)
        for e in errors:
            print(f"  {e}", file=sys.stderr)
        return 1
    print(f"all {found} reads outside a project's folder are declared inputs")
    return 0


if __name__ == "__main__":
    sys.exit(main())
