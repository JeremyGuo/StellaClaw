#!/usr/bin/env python3
from __future__ import annotations

import argparse
import pathlib
import re
import sys


SEMVER_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?$")


def load_version_file(path: pathlib.Path) -> tuple[str, str]:
    text = path.read_text(encoding="utf-8")
    match = re.search(r"(?ms)^---\n(.*?)\n---\n", text)
    if match:
        meta_block = match.group(1)
        body = text[match.end() :].lstrip()
    else:
        shorthand_match = re.match(
            r"(?ms)^(version\s*:\s*.+?)\n---\n",
            text,
        )
        if not shorthand_match:
            raise ValueError("VERSION file is missing frontmatter")
        meta_block = shorthand_match.group(1)
        body = text[shorthand_match.end() :].lstrip()

    meta = {}
    for raw_line in meta_block.splitlines():
        line = raw_line.strip()
        if not line or ":" not in line:
            continue
        key, value = line.split(":", 1)
        meta[key.strip()] = value.strip()

    version = meta.get("version")
    if not version:
        raise ValueError("VERSION frontmatter is missing 'version'")
    if not SEMVER_RE.match(version):
        raise ValueError(
            f"VERSION frontmatter must use SemVer or SemVer prerelease, got '{version}'"
        )

    return version, body


def extract_release_notes(version: str, body: str) -> str:
    heading = f"# {version}"
    lines = body.splitlines()
    start = None
    for index, line in enumerate(lines):
        if line.strip() == heading:
            start = index + 1
            break
    if start is None:
        raise ValueError(f"VERSION body is missing heading '{heading}'")

    collected: list[str] = []
    for line in lines[start:]:
        if line.startswith("# "):
            break
        collected.append(line)

    notes = "\n".join(collected).strip()
    if not notes:
        raise ValueError(f"VERSION heading '{heading}' has no release notes")
    return notes + "\n"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "command",
        choices=["version", "notes", "check"],
        help="What to print from VERSION",
    )
    parser.add_argument(
        "--file",
        default="VERSION",
        help="Path to VERSION markdown file",
    )
    args = parser.parse_args()

    version, body = load_version_file(pathlib.Path(args.file))

    if args.command == "version":
        sys.stdout.write(version + "\n")
    elif args.command == "notes":
        sys.stdout.write(extract_release_notes(version, body))
    else:
        extract_release_notes(version, body)
        sys.stdout.write("ok\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
