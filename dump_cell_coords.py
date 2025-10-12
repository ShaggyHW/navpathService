#!/usr/bin/env python3
import argparse
import sqlite3
from shapely import wkb

def fetch_coords(db_path: str, cell_id: int):
    conn = sqlite3.connect(db_path)
    cur = conn.cursor()
    row = cur.execute("SELECT plane, kind, wkb FROM cells WHERE id=?", (cell_id,)).fetchone()
    if row is None:
        raise SystemExit(f"Cell {cell_id} not found.")
    plane, kind, blob = row
    geom = wkb.loads(blob)
    coords = list(geom.exterior.coords) if geom.geom_type == "Polygon" else [list(p.exterior.coords) for p in geom.geoms]
    return plane, kind, coords

if __name__ == "__main__":
    ap = argparse.ArgumentParser(description="Dump navmesh cell coordinates from navmesh.db.")
    ap.add_argument("--db", required=True, help="Path to navmesh.db")
    ap.add_argument("--cell-id", type=int, required=True, help="Cell ID to inspect")
    ap.add_argument("--out", help="Optional path to write coordinates block")
    args = ap.parse_args()

    plane, kind, coords = fetch_coords(args.db, args.cell_id)
    print(f"plane={plane} kind={kind}")
    print("coordinates:")
    print("")
    block_lines = ["Coordinate[] path = {"]
    if isinstance(coords[0], tuple):
        for x, y in coords:
            ix, iy = int(round(x)), int(round(y))
            block_lines.append(f" new Coordinate ({ix}, {iy}),")
    else:
        for ring_idx, ring in enumerate(coords):
            # print(f"  ring {ring_idx}:")
            for x, y in ring:
                ix, iy = int(round(x)), int(round(y))
                block_lines.append(f"    new Coordinate ({ix}, {iy}),")
    block_lines.append("}")

    for line in block_lines:
        print(line)

    if args.out:
        with open(args.out, "w", encoding="utf-8") as fh:
            fh.write("\n".join(block_lines))
            fh.write("\n")
