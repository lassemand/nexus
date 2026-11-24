#!/usr/bin/env python3
"""Checks that moon's project graph has the same crate dependencies as Cargo.

moon's Rust toolchain plugin turns `path = ...` dependencies in each project's
Cargo.toml into project dependencies. This compares them with
`cargo metadata --no-deps` of the root workspace and of the separate
workspaces (verdict/bench, verdict/template), and fails on any difference, so
that the plugin cannot silently miss an edge (docs/guides/ci.md).

Rules:
  * every member of a Cargo workspace checked here, and every workspace root,
    is a moon project with that folder as its source;
  * a package that is not a project itself (the bench kernels) belongs to the
    deepest project that contains it;
  * every path dependency (normal, dev, build, any target) between packages of
    two different projects is a moon dependency of the first on the second,
    and moon has no other dependency between Rust projects.

Usage: scripts/ci/check-moon-graph.py   (needs cargo and moon on PATH)
"""

import json
import os
import subprocess
import sys

WORKSPACES = ["Cargo.toml", "verdict/bench/Cargo.toml", "verdict/template/Cargo.toml"]


def run_json(args, cwd):
    out = subprocess.run(args, cwd=cwd, check=True, capture_output=True, text=True).stdout
    return json.loads(out)


def main():
    root = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"], check=True, capture_output=True, text=True
    ).stdout.strip()
    moon = os.environ.get("MOON", "moon")

    projects = run_json([moon, "query", "projects"], root)["projects"]
    source_of = {p["id"]: p["source"] for p in projects}
    rust_projects = {p["id"] for p in projects if "rust" in (p.get("toolchains") or []) and p["source"] != "."}
    moon_edges = {
        (p["id"], d["id"])
        for p in projects
        if p["id"] in rust_projects
        for d in p.get("dependencies") or []
        if d["id"] in rust_projects
    }

    def rel(path):
        return os.path.relpath(path, root).replace(os.sep, "/")

    def owner(package_dir):
        best = None
        for pid in rust_projects:
            src = source_of[pid]
            if package_dir == src or package_dir.startswith(src + "/"):
                if best is None or len(src) > len(source_of[best]):
                    best = pid
        return best

    errors = []
    packages = {}
    exact = set()
    for manifest in WORKSPACES:
        meta = run_json(
            ["cargo", "metadata", "--no-deps", "--format-version", "1", "--manifest-path", manifest], root
        )
        exact.add(rel(meta["workspace_root"]))
        members = set(meta["workspace_members"])
        for pkg in meta["packages"]:
            pkg_dir = rel(os.path.dirname(pkg["manifest_path"]))
            if manifest == "Cargo.toml" and pkg["id"] in members:
                exact.add(pkg_dir)
            packages[pkg_dir] = pkg

    for pkg_dir in sorted(exact - {"."}):
        if pkg_dir not in source_of.values():
            errors.append(f"{pkg_dir}: a Cargo package or workspace that is not a moon project")

    cargo_edges = set()
    for pkg_dir, pkg in sorted(packages.items()):
        src = owner(pkg_dir)
        if src is None:
            errors.append(f"{pkg_dir}: package {pkg['name']} is in no moon project")
            continue
        for dep in pkg["dependencies"]:
            if not dep.get("path"):
                continue
            dst = owner(rel(dep["path"]))
            if dst is None:
                errors.append(f"{pkg_dir}: path dependency {dep['name']} ({rel(dep['path'])}) is in no moon project")
            elif dst != src:
                cargo_edges.add((src, dst))

    for a, b in sorted(cargo_edges - moon_edges):
        errors.append(f"moon is missing {a} -> {b} (a path dependency in Cargo)")
    for a, b in sorted(moon_edges - cargo_edges):
        errors.append(f"moon has {a} -> {b}, Cargo has no such path dependency")

    if errors:
        print("moon's project graph differs from Cargo:", file=sys.stderr)
        for e in errors:
            print(f"  {e}", file=sys.stderr)
        return 1
    print(f"moon and Cargo agree: {len(rust_projects)} Rust projects, {len(cargo_edges)} dependencies")
    return 0


if __name__ == "__main__":
    sys.exit(main())
