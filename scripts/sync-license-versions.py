#!/usr/bin/env python3
"""Sync workspace crate versions in both license notices without reordering them.

Run with Python 3.11+. For dependency changes, regenerate with cargo-about on
Linux or WSL as documented in about.hbs instead.
"""

import argparse
from pathlib import Path
import re
import tomllib

ROOT = Path(__file__).resolve().parent.parent
NOTICES = ("Third-Party-License.md", "Third-Party-License-non-GPL.md")
USED_BY = re.compile(r"^Used by:\r?\n(?:- [^\r\n]+\r?\n)+", re.MULTILINE)
CRATE = re.compile(r"^- ([\w-]+) (\S+)(?=\r?$)", re.MULTILINE)


def notice_updates() -> list[tuple[Path, bytes]]:
    root = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    package = root["package"]
    versions = {package["name"]: package["version"]}
    for member in root["workspace"]["members"]:
        manifest = ROOT / member / "Cargo.toml"
        crate = tomllib.loads(manifest.read_text(encoding="utf-8"))["package"]
        versions[crate["name"]] = crate["version"]

    changes = []
    for filename in NOTICES:
        path = ROOT / filename
        original = path.read_bytes().decode("utf-8")
        seen = set()

        def replace(match: re.Match[str]) -> str:
            name = match[1]
            if name not in versions:
                return match[0]
            seen.add(name)
            return f"- {name} {versions[name]}"

        updated = USED_BY.sub(lambda block: CRATE.sub(replace, block[0]), original)
        if package["name"] not in seen:
            raise ValueError(f"{filename}: missing {package['name']} license entry; regenerate the notices")
        if updated != original:
            changes.append((path, updated.encode("utf-8")))
    return changes


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    parser.add_argument("--check", action="store_true", help="exit 1 if a notice needs updating, without writing it")
    args = parser.parse_args()
    try:
        changes = notice_updates()
        for path, data in changes:
            if args.check:
                print(f"Needs update: {path.name}")
            else:
                path.write_bytes(data)
                print(f"Updated: {path.name}")
    except (OSError, ValueError, KeyError) as error:
        parser.exit(1, f"{parser.prog}: {error}\n")

    if not changes:
        print("License notice versions are up to date.")
    return int(args.check and bool(changes))


if __name__ == "__main__":
    raise SystemExit(main())
