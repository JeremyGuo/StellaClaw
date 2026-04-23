#!/usr/bin/env python3
"""Read release version metadata from the root VERSION file."""

from __future__ import annotations

import argparse
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
VERSION_FILE = ROOT / "VERSION"


def read_version_file() -> tuple[str, str]:
    raw = VERSION_FILE.read_text(encoding="utf-8")
    header, sep, body = raw.partition("---")
    if not sep:
        raise SystemExit("VERSION is missing the --- separator")

    version = None
    for line in header.splitlines():
        key, colon, value = line.partition(":")
        if colon and key.strip() == "version":
            version = value.strip()
            break
    if not version:
        raise SystemExit("VERSION is missing a version: header")

    return version, extract_current_notes(body, version)


def extract_current_notes(body: str, version: str) -> str:
    lines = body.strip().splitlines()
    start = None
    for index, line in enumerate(lines):
        if line.strip() == f"# {version}":
            start = index
            break
    if start is None:
        raise SystemExit(f"VERSION is missing changelog section # {version}")

    end = len(lines)
    for index in range(start + 1, len(lines)):
        line = lines[index]
        if line.startswith("# ") and line.strip() != f"# {version}":
            end = index
            break

    return "\n".join(lines[start:end]).strip()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("field", choices=["version", "notes"])
    args = parser.parse_args()

    version, notes = read_version_file()
    if args.field == "version":
        print(version)
    else:
        print(notes)


if __name__ == "__main__":
    main()
