#!/usr/bin/env python3
"""Bump the [package] version in Cargo.toml and print the new version.

    python scripts/bump_version.py [patch|minor|major]   (default: patch)

Only the `version = "..."` line inside [package] is touched, so dependency
versions and formatting are left alone. Run `cargo update --workspace`
afterwards to refresh Cargo.lock.
"""
import re
import sys
from pathlib import Path

part = sys.argv[1] if len(sys.argv) > 1 else "patch"
if part not in ("patch", "minor", "major"):
    sys.exit(f"unknown bump {part!r}: use patch, minor or major")

path = Path(__file__).resolve().parent.parent / "Cargo.toml"
text = path.read_text(encoding="utf-8")

# [package] runs from its header to the next table header.
section = re.search(r"(?ms)^\[package\]\s*$(.*?)(?=^\[|\Z)", text)
if not section:
    sys.exit("no [package] section in Cargo.toml")
line = re.search(r'(?m)^version\s*=\s*"(\d+)\.(\d+)\.(\d+)"', section.group(1))
if not line:
    sys.exit("no version = \"X.Y.Z\" in [package]")

major, minor, patch = map(int, line.groups())
if part == "major":
    major, minor, patch = major + 1, 0, 0
elif part == "minor":
    minor, patch = minor + 1, 0
else:
    patch += 1
new = f"{major}.{minor}.{patch}"

start = section.start(1) + line.start()
end = section.start(1) + line.end()
path.write_text(text[:start] + f'version = "{new}"' + text[end:], encoding="utf-8")
print(new)
