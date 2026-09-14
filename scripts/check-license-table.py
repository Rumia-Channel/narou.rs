#!/usr/bin/env python3
"""Verify the hand-written direct-dependency table in `about.hbs`.

Sections 4 and 5 of the license notices are generated from the locked Cargo
graph, so CI regenerating them catches dependency drift. The table in section 3
is written by hand, and regeneration alone would happily keep a stale table, so
this script compares it against the manifest instead.

Usage: python3 scripts/check-license-table.py   (run from the repository root)
"""

from __future__ import annotations

import json
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
TEMPLATE = ROOT / "about.hbs"
NOTICES = ("Third-Party-License.md", "Third-Party-License-non-GPL.md")

# Table markers that map a row to a dependency kind. Everything else is a
# normal dependency.
MARKERS = {
    " (build)": "build",
    " (dev)": "dev",
    " (Windows)": "windows",
    " (`lite` feature only)": "normal",
}

ROW = re.compile(r"^\| ([^|]+?) \| ([^|]+?) \| ([^|]+?) \|$")


def table_rows() -> list[tuple[str, str, str, str]]:
    """Return (name, kind, version req, license) for every row in section 3."""
    text = TEMPLATE.read_text(encoding="utf-8")
    section = text.split("## 3. Direct Rust dependencies", 1)[1].split("## 4.", 1)[0]
    rows = []
    for line in section.splitlines():
        match = ROW.match(line.strip())
        if not match:
            continue
        name, req, license_ = (group.strip() for group in match.groups())
        if name in ("Dependency", "---"):
            continue
        kind = "normal"
        for marker, marked_kind in MARKERS.items():
            if name.endswith(marker):
                name = name[: -len(marker)]
                kind = marked_kind
                break
        rows.append((name, kind, req, license_))
    return rows


def manifest_deps() -> dict[tuple[str, str], str]:
    """Return {(name, kind): version req} for narou_rs, kind as in the table."""
    raw = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--all-features"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    metadata = json.loads(raw)
    deps: dict[tuple[str, str], str] = {}
    for package in metadata["packages"]:
        if package["name"] != "narou_rs":
            continue
        for dep in package["dependencies"]:
            kind = dep["kind"] or "normal"
            if kind == "normal" and "windows" in (dep["target"] or ""):
                kind = "windows"
            deps[(dep["name"], kind)] = dep["req"]
    return deps


def main() -> int:
    rows = table_rows()
    manifest = manifest_deps()
    failures: list[str] = []

    for name, kind, req, _license in rows:
        found = manifest.get((name, kind))
        if found is None:
            failures.append(f"about.hbs lists {name} ({kind}) but the manifest does not")
        elif found != "*" and req != found:
            failures.append(f"{name} ({kind}): about.hbs says `{req}`, manifest says `{found}`")

    listed = {(name, kind) for name, kind, _req, _license in rows}
    for name, kind in sorted(manifest):
        if (name, kind) not in listed:
            failures.append(f"Cargo.toml has {name} ({kind}) {manifest[(name, kind)]} but about.hbs omits it")

    # Every crate named in the table must appear in the generated notices, so a
    # rename or a removal cannot leave the table pointing at nothing.
    text = "\n".join((ROOT / notice).read_text(encoding="utf-8") for notice in NOTICES)
    for name, _kind, _req, _license in rows:
        if not re.search(rf"^- {re.escape(name)} \S+$", text, re.M):
            failures.append(f"{name} is not listed in the generated notices")

    if failures:
        print("about.hbs section 3 is out of date:", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1

    print(f"about.hbs section 3 matches the manifest ({len(rows)} direct dependencies)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
