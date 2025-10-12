#!/usr/bin/env python3
import argparse
import sqlite3
from shapely import wkb


def fetch_coords(db_path: str, cell_id: int=None):
    conn = sqlite3.connect(db_path)
    cur = conn.cursor()
    if cell_id is None:
        rows = cur.execute("SELECT id, plane, kind, wkb FROM cells ORDER BY id").fetchall()
        if not rows:
            raise SystemExit("No cells found.")
    else:
        rows = cur.execute("SELECT id, plane, kind, wkb FROM cells WHERE id=?", (cell_id,)).fetchall()
        if not rows:
            raise SystemExit(f"Cell {cell_id} not found.")
    result = []
    for cid, plane, kind, blob in rows:
        geom = wkb.loads(blob)
        coords = list(geom.exterior.coords) if geom.geom_type == "Polygon" else [list(p.exterior.coords) for p in geom.geoms]
        result.append((cid, plane, kind, coords))
    conn.close()
    return result


def emit_block(cid: int, plane: int, kind: str, coords):
    print(f"cell_id={cid} plane={plane} kind={kind}")
    print("coordinates:")
    print("")
    block_lines = ["Area area = new Area.Polygonal("]
    if isinstance(coords[0], tuple):
        for x, y in coords:
            ix, iy = int(round(x)), int(round(y))
            block_lines.append(f" new Coordinate ({ix}, {iy}),")
    else:
        for ring_idx, ring in enumerate(coords):
            for x, y in ring:
                ix, iy = int(round(x)), int(round(y))
                block_lines.append(f"    new Coordinate ({ix}, {iy}),")
    block_lines.append(")")
    for line in block_lines:
        print(line)
    return block_lines

if __name__ == "__main__":
    ap = argparse.ArgumentParser(description="Dump navmesh cell coordinates from navmesh.db.")
    ap.add_argument("--db", required=True, help="Path to navmesh.db")
    ap.add_argument("--cell-id", type=int, required=False, help="Cell ID to inspect")
    ap.add_argument("--out", help="Optional path to write coordinates block")
    args = ap.parse_args()
    entries = fetch_coords(args.db, args.cell_id)
    all_blocks = []
    for idx, (cid, plane, kind, coords) in enumerate(entries):
        if idx > 0:
            print("")
        block_lines = emit_block(cid, plane, kind, coords)
        all_blocks.append("\n".join(block_lines))
    if args.out:
        with open(args.out, "w", encoding="utf-8") as fh:
            fh.write("\n\n".join(all_blocks))
            fh.write("\n")
