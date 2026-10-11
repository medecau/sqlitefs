#!/usr/bin/env python3
"""Run undo-dedup.sql on an unmounted sqlitefs database.

Usage: python3 scripts/undo-dedup.py <db_path>

Supplies the unchunk(data, compressed) SQL function the script needs: a chunk's
bytes, zstd-decompressed when compressed is 1. Needs Python 3.14+ (compression.zstd).
"""

import sqlite3
import sys
from pathlib import Path

from compression import zstd

if len(sys.argv) != 2:
    sys.exit(__doc__)
con = sqlite3.connect(sys.argv[1], autocommit=True)
con.create_function(
    "unchunk",
    2,
    lambda data, compressed: zstd.decompress(data) if compressed else data,
    deterministic=True,
)
con.executescript(Path(__file__).with_name("undo-dedup.sql").read_text())
con.close()
