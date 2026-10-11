#!/usr/bin/env python3
"""Fail if the bundled SQLite predates a fix listed on sqlite.org/cves.html.

`cargo audit` (RustSec) and GitHub's advisory database do not track SQLite's
own CVEs against libsqlite3-sys, so the bundled C library is checked directly
against SQLite's CVE table. Rows whose Fix column is not a version ("Not a
bug in SQLite") are skipped.

Usage: check-sqlite-cves.py [CVE-ID ...]   # listed CVE IDs are ignored
Run from the repository root (uses `cargo metadata` and Cargo.lock).
"""

import html
import json
import os
import re
import subprocess
import sys
import urllib.request
from collections.abc import Iterator
from pathlib import Path

URL = "https://www.sqlite.org/cves.html"


def version(text: str) -> tuple[int, ...]:
    return tuple(int(n) for n in text.split("."))


def bundled_sqlite() -> str:
    meta = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--format-version", "1", "--locked"]
        )
    )
    pkg = next(p for p in meta["packages"] if p["name"] == "libsqlite3-sys")
    header = Path(pkg["manifest_path"]).parent / "sqlite3" / "sqlite3.h"
    m = re.search(r'#define SQLITE_VERSION\s+"([\d.]+)"', header.read_text())
    if not m:
        sys.exit(f"SQLITE_VERSION not found in {header}")
    return m.group(1)


def cve_rows(page: str) -> Iterator[tuple[list[str], str]]:
    for row in re.findall(r"<tr[^>]*>(.*?)</tr>", page, re.DOTALL):
        cells = [
            html.unescape(re.sub(r"<[^>]+>", "", c)).strip()
            for c in re.findall(r"<td[^>]*>(.*?)</td>", row, re.DOTALL)
        ]
        if len(cells) >= 2 and "CVE-" in cells[0]:
            yield re.findall(r"CVE-\d+-\d+", cells[0]), cells[1]


def main() -> None:
    ignored = set(sys.argv[1:])
    bundled = bundled_sqlite()
    req = urllib.request.Request(URL, headers={"User-Agent": "sqlitefs-ci"})
    page = urllib.request.urlopen(req, timeout=30).read().decode("utf-8", "replace")
    rows = list(cve_rows(page))
    if len(rows) < 10:  # page layout changed: fail loudly rather than pass silently
        sys.exit(f"parsed only {len(rows)} CVE rows from {URL}; update this script")

    open_cves = []
    for cves, fix in rows:
        m = re.match(r"(\d+(?:\.\d+)+)", fix)
        if m and version(m.group(1)) > version(bundled):
            open_cves += [(c, m.group(1)) for c in cves if c not in ignored]

    print(f"bundled SQLite {bundled}; {len(rows)} CVE rows on {URL}")
    annotate = os.environ.get("GITHUB_ACTIONS") == "true"
    for cve, fix in open_cves:
        msg = f"{cve}: fixed in SQLite {fix}, bundled is {bundled}"
        print(f"::error title=SQLite {cve}::{msg}" if annotate else msg)
    sys.exit(1 if open_cves else 0)


if __name__ == "__main__":
    main()
