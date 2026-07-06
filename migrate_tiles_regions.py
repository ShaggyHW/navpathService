#!/usr/bin/env python3
"""Create the compact tiles_regions table from the row-per-tile tiles table.

Blob layout per (region, plane): 512-byte presence bitmap + 4096 walk_mask bytes,
row-major within the 64x64 region (i = (y - base_y) * 64 + (x - base_x)). A presence
bitmap is required because walk_mask == 0 is a legal value (teleport-only tiles).

The builder prefers this table when present (~2.8k rows instead of millions); the
row-per-tile table remains the fallback and can be dropped once the producer emits
tiles_regions directly. Re-run after any tiles change.
"""
import sqlite3
import sys

db = sys.argv[1] if len(sys.argv) > 1 else "worldReachableTiles.db"
con = sqlite3.connect(db)
cur = con.cursor()

cur.execute("DROP TABLE IF EXISTS tiles_regions")
cur.execute(
    """CREATE TABLE tiles_regions (
        plane INTEGER NOT NULL,
        base_x INTEGER NOT NULL,
        base_y INTEGER NOT NULL,
        blob BLOB NOT NULL,
        PRIMARY KEY (plane, base_y, base_x)
    ) WITHOUT ROWID"""
)

regions = {}
for x, y, plane, mask in cur.execute("SELECT x, y, plane, walk_mask FROM tiles"):
    key = (plane, x // 64 * 64, y // 64 * 64)
    blob = regions.get(key)
    if blob is None:
        blob = bytearray(512 + 4096)
        regions[key] = blob
    i = (y % 64) * 64 + (x % 64)
    blob[i // 8] |= 1 << (i % 8)
    blob[512 + i] = mask & 0xFF

cur.executemany(
    "INSERT INTO tiles_regions (plane, base_x, base_y, blob) VALUES (?, ?, ?, ?)",
    [(p, bx, by, bytes(b)) for (p, bx, by), b in sorted(regions.items())],
)
con.commit()
n = cur.execute("SELECT COUNT(*) FROM tiles_regions").fetchone()[0]
t = cur.execute("SELECT COUNT(*) FROM tiles").fetchone()[0]
print(f"tiles_regions: {n} region rows covering {t} tiles")
con.execute("VACUUM")
con.close()
