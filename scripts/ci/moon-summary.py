#!/usr/bin/env python3
"""Writes what scripts/ci/moon-ci.sh did as Markdown, for the GitHub job summary.

Reads mode, touched and report.json in .moon/cache/moon-ci/ (written by
moon-ci.sh; report.json is moon's own report of the run): whether only
affected tasks ran or everything (and why), the touched files, every task moon
ran with its result and time, and the tasks it did not run.

Usage: scripts/ci/moon-summary.py [title] [target...]
The targets limit the "not run" list (default: every task that runs in CI).
Appends to $GITHUB_STEP_SUMMARY when set, else prints.
"""

import json
import os
import subprocess
import sys

MAX_FILES = 200


def main():
    root = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"], check=True, capture_output=True, text=True
    ).stdout.strip()
    os.chdir(root)
    title = sys.argv[1] if len(sys.argv) > 1 else "moon ci"
    targets = sys.argv[2:]
    state = ".moon/cache/moon-ci"
    out = [f"## {title}", ""]

    mode = read(f"{state}/mode")
    touched = [f for f in (read(f"{state}/touched") or "").splitlines() if f]
    if mode is None:
        out.append("**moon-ci.sh did not record a mode: it failed before choosing what to run.**")
    elif mode == "affected":
        out.append(f"Ran the tasks affected by **{len(touched)} touched files** (deep dependents).")
    else:
        out.append(f"Ran **every task**: {mode.removeprefix('everything: ')}.")
    out.append("")

    ran = {}
    report = read(f"{state}/report.json")
    if report is None:
        out.append("moon ran no task (none was affected, or moon failed before starting one).")
    else:
        for action in json.loads(report).get("actions", []):
            node = action.get("node", {})
            if node.get("action") != "run-task":
                continue
            target = node["params"]["target"]
            secs = (action.get("duration") or {}).get("secs")
            ran[target] = (action.get("status", "?"), secs)

    if ran:
        out += ["| Task | Result | Time |", "|---|---|---|"]
        for target, (status, secs) in sorted(ran.items()):
            time = f"{secs // 60}m{secs % 60:02d}s" if isinstance(secs, int) else ""
            out.append(f"| `{target}` | {status} | {time} |")
        out.append("")

    universe = targets or ci_tasks()
    skipped = sorted(t for t in universe if t not in ran)
    if universe:
        out.append(f"<details><summary>Not run: {len(skipped)} of {len(universe)} tasks</summary>\n")
        out += [f"- `{t}`" for t in skipped] or ["(none)"]
        out += ["", "</details>", ""]

    if touched:
        shown = touched[:MAX_FILES]
        out.append(f"<details><summary>Touched files ({len(touched)})</summary>\n")
        out += [f"- `{f}`" for f in shown]
        if len(touched) > MAX_FILES:
            out.append(f"- … and {len(touched) - MAX_FILES} more")
        out += ["", "</details>", ""]

    text = "\n".join(out) + "\n"
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if path:
        with open(path, "a", encoding="utf-8") as f:
            f.write(text)
    else:
        sys.stdout.write(text)


def read(path):
    try:
        with open(path, encoding="utf-8") as f:
            return f.read().strip()
    except OSError:
        return None


def ci_tasks():
    try:
        out = subprocess.run(
            [os.environ.get("MOON", "moon"), "query", "tasks"], check=True, capture_output=True, text=True
        ).stdout
    except (OSError, subprocess.CalledProcessError):
        return []
    tasks = json.loads(out).get("tasks", {})
    return [
        t["target"]
        for project in tasks.values()
        for t in project.values()
        if t.get("options", {}).get("runInCI") not in (False, "skip")
    ]


if __name__ == "__main__":
    main()
