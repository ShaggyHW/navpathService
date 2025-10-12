
#!/usr/bin/env python3
"""
NavPath RS3 — Dynamic Polygonal NavMesh Builder
===============================================

Creates a polygonal navmesh from the `tiles` table and writes it to a separate SQLite DB,
with off-mesh overlay links from node tables (doors, lodestones, objects, NPCs, IF slots, items).

Defaults to polygonal cells to minimize overlap; triangulation is **optional**.

Dependencies
------------
  pip install shapely rtree

Usage
-----
  python navmesh_builder.py \
    --input worldReachableTiles.db \
    --output navmesh.db \
    --mode polys \
    --triangulate  # (optional) if you want triangles + funnel-ready portals
    --bbox "plane=0,xmin=3000,xmax=3400,ymin=3300,ymax=3600"

Notes
-----
* Polygons are produced by merging 1x1 tile squares via Shapely unary_union.
* Portals (adjacency edges) are computed for both polygons and triangles by
  intersecting cell boundaries; only segment overlaps become portals.
* Off-mesh links land you *inside* destination areas (or at door endpoints).
"""

import argparse
import json
import math
import sqlite3
import sys
from typing import Iterable, List, Tuple, Dict, Optional

try:
    from shapely.geometry import box, Polygon, Point, MultiPolygon, LineString
    from shapely.ops import unary_union, triangulate
    from shapely import wkb
except Exception as e:
    print("Error: shapely is required. Install with: pip install shapely", file=sys.stderr)
    raise

try:
    from rtree import index as rtree_index
    HAS_RTREE = True
except Exception:
    HAS_RTREE = False


# -------------------------
# SQL helpers & constants
# -------------------------

TILES_SELECT_BBOX = """
SELECT x, y, plane, allowed_directions, blocked_directions
FROM tiles
WHERE plane = :plane
  AND x BETWEEN :xmin AND :xmax
  AND y BETWEEN :ymin AND :ymax
"""

TILES_SELECT_PLANE = """
SELECT x, y, plane, allowed_directions, blocked_directions
FROM tiles
WHERE plane = :plane
"""

DISTINCT_PLANES_SQL = "SELECT DISTINCT plane FROM tiles"

NODE_TABLES = {
    "object_nodes": {
        "key": "object",
        "has_origin": True,
        "cols": ("id","dest_min_x","dest_max_x","dest_min_y","dest_max_y","dest_plane",
                 "orig_min_x","orig_max_x","orig_min_y","orig_max_y","orig_plane",
                 "search_radius","cost","next_node_type","next_node_id","requirement_id"),
        "sql": """
            SELECT id, dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane,
                   orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane,
                   search_radius, cost, next_node_type, next_node_id, requirement_id
            FROM object_nodes
            WHERE dest_plane IS NOT NULL
        """
    },
    "npc_nodes": {
        "key": "npc",
        "has_origin": True,
        "cols": ("id","match_type","npc_id","npc_name","action",
                 "dest_min_x","dest_max_x","dest_min_y","dest_max_y","dest_plane",
                 "search_radius","cost","orig_min_x","orig_max_x","orig_min_y","orig_max_y","orig_plane",
                 "next_node_type","next_node_id","requirement_id"),
        "sql": """
            SELECT id, match_type, npc_id, npc_name, action,
                   dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane,
                   search_radius, cost, orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane,
                   next_node_type, next_node_id, requirement_id
            FROM npc_nodes
            WHERE dest_plane IS NOT NULL
        """
    },
    "ifslot_nodes": {
        "key": "ifslot",
        "has_origin": False,
        "cols": ("id","interface_id","component_id","slot_id","click_id",
                 "dest_min_x","dest_max_x","dest_min_y","dest_max_y","dest_plane",
                 "cost","next_node_type","next_node_id","requirement_id"),
        "sql": """
            SELECT id, interface_id, component_id, slot_id, click_id,
                   dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane,
                   cost, next_node_type, next_node_id, requirement_id
            FROM ifslot_nodes
            WHERE dest_plane IS NOT NULL
        """
    },
    "item_nodes": {
        "key": "item",
        "has_origin": False,
        "cols": ("id","item_id","action",
                 "dest_min_x","dest_max_x","dest_min_y","dest_max_y","dest_plane",
                 "cost","next_node_type","next_node_id","requirement_id"),
        "sql": """
            SELECT id, item_id, action,
                   dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane,
                   cost, next_node_type, next_node_id, requirement_id
            FROM item_nodes
            WHERE dest_plane IS NOT NULL
        """
    },
    "lodestone_nodes": {
        "key": "lodestone",
        "has_point_dest": True,
        "cols": ("id","lodestone","dest_x","dest_y","dest_plane","cost","next_node_type","next_node_id","requirement_id"),
        "sql": """
            SELECT id, lodestone, dest_x, dest_y, dest_plane, cost, next_node_type, next_node_id, requirement_id
            FROM lodestone_nodes
        """
    },
    "door_nodes": {
        "key": "door",
        "has_tiles": True,
        "cols": ("id","direction","real_id_open","real_id_closed",
                 "location_open_x","location_open_y","location_open_plane",
                 "location_closed_x","location_closed_y","location_closed_plane",
                 "tile_inside_x","tile_inside_y","tile_inside_plane",
                 "tile_outside_x","tile_outside_y","tile_outside_plane",
                 "open_action","cost","next_node_type","next_node_id","requirement_id"),
        "sql": """
            SELECT id, direction, real_id_open, real_id_closed,
                   location_open_x, location_open_y, location_open_plane,
                   location_closed_x, location_closed_y, location_closed_plane,
                   tile_inside_x, tile_inside_y, tile_inside_plane,
                   tile_outside_x, tile_outside_y, tile_outside_plane,
                   open_action, cost, next_node_type, next_node_id, requirement_id
            FROM door_nodes
        """
    },
}


# -------------------------
# Geometry helpers
# -------------------------

DIR_TOKENS = {"N","NE","E","SE","S","SW","W","NW","NORTH","EAST","SOUTH","WEST","NORTHEAST","NORTHWEST","SOUTHEAST","SOUTHWEST"}

def is_tile_walkable(allowed: Optional[str], blocked: Optional[str]) -> bool:
    """
    Conservative walkability: a tile is walkable if it has *any* allowed directions token.
    If allowed is NULL/empty, it's considered non-walkable.
    """
    if not allowed:
        return False
    tokens = {t.strip().upper() for t in allowed.split(",") if t.strip()}
    return len(tokens & DIR_TOKENS) > 0


def tiles_to_polygons(tiles: Iterable[Tuple[int,int]]) -> List[Polygon]:
    """
    Convert iterable of (x,y) integer tile coords into a list of merged Polygons.
    """
    boxes = [box(x, y, x+1, y+1) for (x,y) in tiles]
    if not boxes:
        return []
    merged = unary_union(boxes)
    if isinstance(merged, Polygon):
        return [merged]
    if isinstance(merged, MultiPolygon):
        return [p for p in merged.geoms if isinstance(p, Polygon)]
    # Fallback: filter polys from collections
    return [g for g in getattr(merged, "geoms", []) if isinstance(g, Polygon)]


def triangles_from_polygon(poly: Polygon) -> List[Polygon]:
    """
    Triangulate polygon area; keep only triangles whose centroid lies within the polygon.
    """
    tris = triangulate(poly)
    return [t for t in tris if t.centroid.within(poly)]


def bbox_of_geom(g) -> Tuple[float,float,float,float]:
    minx, miny, maxx, maxy = g.bounds
    return (float(minx), float(miny), float(maxx), float(maxy))


# -------------------------
# RTree wrapper (optional)
# -------------------------

class SimpleRTree:
    def __init__(self):
        self._has = HAS_RTREE
        self._items = {}  # id -> bbox
        if self._has:
            p = rtree_index.Property()
            p.interleaved = True
            self._rt = rtree_index.Index(properties=p)
        else:
            self._rt = None

    def insert(self, id_: int, bbox: Tuple[float,float,float,float]):
        self._items[id_] = bbox
        if self._has:
            self._rt.insert(id_, bbox)

    def intersects(self, bbox: Tuple[float,float,float,float]) -> Iterable[int]:
        if self._has:
            return list(self._rt.intersection(bbox))
        # brute-force fallback
        xmin, ymin, xmax, ymax = bbox
        out = []
        for id_, (x0,y0,x1,y1) in self._items.items():
            if (x0 <= xmax and x1 >= xmin and y0 <= ymax and y1 >= ymin):
                out.append(id_)
        return out


# -------------------------
# Output DB schema
# -------------------------

CREATE_SCHEMA = """
PRAGMA journal_mode=WAL;
PRAGMA synchronous=OFF;

CREATE TABLE IF NOT EXISTS meta(
  key TEXT PRIMARY KEY,
  value TEXT
);

CREATE TABLE IF NOT EXISTS cells(
  id INTEGER PRIMARY KEY,
  plane INTEGER NOT NULL,
  kind TEXT NOT NULL,             -- 'polygon' or 'triangle'
  wkb BLOB NOT NULL,
  area REAL NOT NULL,
  minx REAL NOT NULL, miny REAL NOT NULL, maxx REAL NOT NULL, maxy REAL NOT NULL
);

-- RTree with (id, minx, maxx, miny, maxy)
CREATE VIRTUAL TABLE IF NOT EXISTS rtree_cells USING rtree(
  id, minx, maxx, miny, maxy
);

CREATE TABLE IF NOT EXISTS portals(
  id INTEGER PRIMARY KEY,
  plane INTEGER NOT NULL,
  a_id INTEGER NOT NULL,
  b_id INTEGER NOT NULL,
  x1 REAL NOT NULL, y1 REAL NOT NULL,
  x2 REAL NOT NULL, y2 REAL NOT NULL,
  length REAL NOT NULL
);

CREATE TABLE IF NOT EXISTS offmesh_links(
  id INTEGER PRIMARY KEY,
  link_type TEXT NOT NULL,
  node_table TEXT NOT NULL,
  node_id INTEGER NOT NULL,
  requirement_id INTEGER,
  cost REAL,
  plane_from INTEGER,
  plane_to INTEGER NOT NULL,
  src_cell_id INTEGER,
  dst_cell_id INTEGER NOT NULL,
  meta_json TEXT
);

CREATE INDEX IF NOT EXISTS idx_cells_plane ON cells(plane);
CREATE INDEX IF NOT EXISTS idx_portals_plane ON portals(plane);
CREATE INDEX IF NOT EXISTS idx_offmesh_dst ON offmesh_links(dst_cell_id);
"""


# -------------------------
# Builders
# -------------------------

def fetch_planes(conn: sqlite3.Connection, bbox: Optional[Dict[str,int]]) -> List[int]:
    if bbox and "plane" in bbox:
        return [int(bbox["plane"])]
    cur = conn.execute(DISTINCT_PLANES_SQL)
    return [row[0] for row in cur.fetchall()]


def fetch_tiles(conn: sqlite3.Connection, plane: int, bbox: Optional[Dict[str,int]]) -> List[Tuple[int,int]]:
    params = {"plane": plane}
    if bbox:
        params.update({
            "xmin": int(bbox.get("xmin", -1<<30)),
            "xmax": int(bbox.get("xmax",  1<<30)),
            "ymin": int(bbox.get("ymin", -1<<30)),
            "ymax": int(bbox.get("ymax",  1<<30)),
        })
        sql = TILES_SELECT_BBOX
    else:
        sql = TILES_SELECT_PLANE
    tiles = []
    for x, y, plane_v, allow, block in conn.execute(sql, params):
        if is_tile_walkable(allow, block):
            tiles.append((int(x), int(y)))
    return tiles


def ensure_output_db(path: str) -> sqlite3.Connection:
    out = sqlite3.connect(path)
    out.execute("PRAGMA foreign_keys=ON;")
    out.executescript(CREATE_SCHEMA)
    return out


def insert_cell(cur: sqlite3.Cursor, plane: int, kind: str, geom) -> int:
    area = float(geom.area)
    minx, miny, maxx, maxy = bbox_of_geom(geom)
    wkb_bytes = wkb.dumps(geom)
    cur.execute(
        "INSERT INTO cells(plane, kind, wkb, area, minx, miny, maxx, maxy) VALUES(?,?,?,?,?,?,?,?)",
        (plane, kind, sqlite3.Binary(wkb_bytes), area, minx, miny, maxx, maxy)
    )
    new_id = cur.lastrowid
    cur.execute("INSERT INTO rtree_cells(id,minx,maxx,miny,maxy) VALUES(?,?,?,?,?)",
                (new_id, minx, maxx, miny, maxy))
    return new_id


def insert_portal(cur: sqlite3.Cursor, plane: int, a_id: int, b_id: int,
                  x1: float, y1: float, x2: float, y2: float):
    length = math.hypot(x2 - x1, y2 - y1)
    if length <= 1e-6:
        return
    cur.execute(
        "INSERT INTO portals(plane, a_id, b_id, x1, y1, x2, y2, length) VALUES(?,?,?,?,?,?,?,?)",
        (plane, a_id, b_id, x1, y1, x2, y2, length)
    )


def build_adjacency(cells: Dict[int, Polygon], plane: int, cur: sqlite3.Cursor):
    # Spatial index over cell bboxes
    sidx = SimpleRTree()
    for cid, geom in cells.items():
        sidx.insert(cid, bbox_of_geom(geom))

    # For each cell, find neighbors with boundary overlap (LineString segment)
    # To avoid O(n^2) duplicates, only connect cid < nid
    for cid, geom in cells.items():
        bb = bbox_of_geom(geom)
        for nid in sidx.intersects(bb):
            if nid <= cid:
                continue
            ng = cells[nid]
            inter = geom.boundary.intersection(ng.boundary)
            if inter.is_empty:
                continue
            # intersection could be MultiLineString or LineString
            if isinstance(inter, LineString):
                if inter.length > 1e-6:
                    x1, y1 = inter.coords[0]
                    x2, y2 = inter.coords[-1]
                    insert_portal(cur, plane, cid, nid, x1, y1, x2, y2)
                    insert_portal(cur, plane, nid, cid, x1, y1, x2, y2)
            else:
                for g in getattr(inter, "geoms", []):
                    if isinstance(g, LineString) and g.length > 1e-6:
                        x1, y1 = g.coords[0]
                        x2, y2 = g.coords[-1]
                        insert_portal(cur, plane, cid, nid, x1, y1, x2, y2)
                        insert_portal(cur, plane, nid, cid, x1, y1, x2, y2)


def find_cell_containing_point(cur: sqlite3.Cursor, plane: int, pt: Point) -> Optional[int]:
    x = float(pt.x); y = float(pt.y)
    # RTree coarse filter
    for (cid,) in cur.execute("SELECT id FROM rtree_cells WHERE minx<=? AND maxx>=? AND miny<=? AND maxy>=?",
                              (x, x, y, y)):
        (wkb_bytes,) = cur.execute("SELECT wkb FROM cells WHERE id=?", (cid,)).fetchone()
        geom = wkb.loads(wkb_bytes)
        if geom.contains(pt):
            return cid
    return None


def find_cells_intersecting_rect(cur: sqlite3.Cursor, plane: int, rect_poly: Polygon) -> List[int]:
    minx, miny, maxx, maxy = bbox_of_geom(rect_poly)
    hits = []
    for (cid,) in cur.execute("SELECT id FROM rtree_cells WHERE minx<=? AND maxx>=? AND miny<=? AND maxy>=?",
                              (maxx, minx, maxy, miny)):
        (wkb_bytes,) = cur.execute("SELECT wkb FROM cells WHERE id=?", (cid,)).fetchone()
        geom = wkb.loads(wkb_bytes)
        if geom.intersects(rect_poly):
            hits.append(cid)
    return hits


def build_navmesh_for_plane(in_conn: sqlite3.Connection, out_conn: sqlite3.Connection,
                            plane: int, mode: str, triangulate_flag: bool,
                            bbox: Optional[Dict[str,int]]):
    print(f"[plane {plane}] Fetching tiles ...")
    tiles = fetch_tiles(in_conn, plane, bbox)
    if not tiles:
        print(f"[plane {plane}] No walkable tiles.")
        return

    print(f"[plane {plane}] Merging tiles into polygons ...")
    polys = tiles_to_polygons(tiles)

    cur = out_conn.cursor()
    out_conn.execute("BEGIN")

    cell_geoms: Dict[int, Polygon] = {}  # id -> geometry

    if mode == "polys" and not triangulate_flag:
        for poly in polys:
            cid = insert_cell(cur, plane, "polygon", poly)
            cell_geoms[cid] = poly
        print(f"[plane {plane}] Building polygon adjacency (portals) ...")
        build_adjacency(cell_geoms, plane, cur)
    else:
        # Triangulate each polygon; keep triangles within polygon only.
        print(f"[plane {plane}] Triangulating polygons into triangles ...")
        for poly in polys:
            tris = triangles_from_polygon(poly)
            for t in tris:
                cid = insert_cell(cur, plane, "triangle", t)
                cell_geoms[cid] = t
        print(f"[plane {plane}] Building triangle adjacency (portals) ...")
        build_adjacency(cell_geoms, plane, cur)

    out_conn.commit()
    print(f"[plane {plane}] Cells stored: {len(cell_geoms)}")

    # Build overlays / off-mesh links
    print(f"[plane {plane}] Building overlays ...")
    build_overlays_for_plane(in_conn, out_conn, plane)


def clamp_rect(minx: Optional[int], maxx: Optional[int], miny: Optional[int], maxy: Optional[int]) -> Optional[Tuple[int,int,int,int]]:
    if None in (minx, maxx, miny, maxy):
        return None
    if maxx < minx or maxy < miny:
        return None
    return int(minx), int(maxx), int(miny), int(maxy)


def rect_to_poly(ixmin: int, ixmax: int, iymin: int, iymax: int) -> Polygon:
    # Treat tile bounds as inclusive, so +1 on max edges to cover full tiles.
    return box(ixmin, iymin, ixmax + 1, iymax + 1)


def build_overlays_for_plane(in_conn: sqlite3.Connection, out_conn: sqlite3.Connection, plane: int):
    cur = out_conn.cursor()

    # Helper: insert offmesh row
    def add_offmesh(link_type: str, node_table: str, node_id: int, req_id: Optional[int],
                    cost: Optional[float], plane_from: Optional[int], plane_to: int,
                    src_cell_id: Optional[int], dst_cell_id: int, meta: Dict):
        cur.execute("""
            INSERT INTO offmesh_links(link_type,node_table,node_id,requirement_id,cost,
                                      plane_from,plane_to,src_cell_id,dst_cell_id,meta_json)
            VALUES(?,?,?,?,?,?,?,?,?,?)
        """, (link_type, node_table, node_id, req_id, cost,
              plane_from, plane_to, src_cell_id, dst_cell_id, json.dumps(meta, separators=(",",":"))))

    # Lodestones (point destination)
    rows = in_conn.execute(NODE_TABLES["lodestone_nodes"]["sql"])
    for node_id, lodename, dx, dy, dplane, cost, _, _, req_id in rows:
        if dplane != plane:
            continue
        pt = Point(float(dx)+0.5, float(dy)+0.5)
        dst_id = find_cell_containing_point(cur, plane, pt)
        if dst_id is None:
            continue
        # Origin is the start tile only ⇒ src_cell_id NULL
        add_offmesh("lodestone","lodestone_nodes",node_id,req_id,cost,None,plane,None,dst_id,
                    {"lodestone": lodename, "dest_point":[dx,dy,dplane]})

    # Doors (connect inside/outside tiles)
    rows = in_conn.execute(NODE_TABLES["door_nodes"]["sql"])
    for (node_id, direction, real_open, real_closed,
         lox, loy, lop, lcx, lcy, lcp,
         ix, iy, ip, ox, oy, op, open_action, cost, _, _, req_id) in rows:
        # Map tiles to cells on their respective planes
        if ip == plane:
            a_pt = Point(float(ix)+0.5, float(iy)+0.5)
            a_id = find_cell_containing_point(cur, plane, a_pt)
        else:
            a_id = None
        if op == plane:
            b_pt = Point(float(ox)+0.5, float(oy)+0.5)
            b_id = find_cell_containing_point(cur, plane, b_pt)
        else:
            b_id = None

        meta = {"inside":[ix,iy,ip], "outside":[ox,oy,op], "open_action": open_action,
                "real_id_open": real_open, "real_id_closed": real_closed}

        # Same-plane: add both directions
        if ip == op == plane and a_id is not None and b_id is not None:
            add_offmesh("door","door_nodes",node_id,req_id,cost,plane,plane,a_id,b_id,meta)
            add_offmesh("door","door_nodes",node_id,req_id,cost,plane,plane,b_id,a_id,meta)
        else:
            # Cross-plane edges terminate/ originate in this plane as applicable
            if a_id is not None:
                add_offmesh("door","door_nodes",node_id,req_id,cost,ip,op,a_id,b_id,meta)
            if b_id is not None:
                add_offmesh("door","door_nodes",node_id,req_id,cost,op,ip,b_id,a_id,meta)

    # Rectangle-destination nodes
    for table_name in ("object_nodes","npc_nodes","ifslot_nodes","item_nodes"):
        specs = NODE_TABLES[table_name]
        rows = in_conn.execute(specs["sql"])
        for row in rows:
            if table_name == "object_nodes":
                (node_id, dminx, dmaxx, dminy, dmaxy, dplane,
                 ominx, omaxx, ominy, omaxy, oplane,
                 search_radius, cost, _, _, req_id) = row
            elif table_name == "npc_nodes":
                (node_id, match_type, npc_id, npc_name, action,
                 dminx, dmaxx, dminy, dmaxy, dplane,
                 search_radius, cost, ominx, omaxx, ominy, omaxy, oplane,
                 _, _, req_id) = row
            elif table_name == "ifslot_nodes":
                (node_id, interface_id, component_id, slot_id, click_id,
                 dminx, dmaxx, dminy, dmaxy, dplane,
                 cost, _, _, req_id) = row
                ominx=omaxx=ominy=omaxy=oplane=None
            else:  # item_nodes
                (node_id, item_id, action,
                 dminx, dmaxx, dminy, dmaxy, dplane,
                 cost, _, _, req_id) = row
                ominx=omaxx=ominy=omaxy=oplane=None

            if dplane != plane:
                continue

            drect = clamp_rect(dminx, dmaxx, dminy, dmaxy)
            if drect is None:
                continue
            dpoly = rect_to_poly(*drect)
            dst_ids = find_cells_intersecting_rect(cur, plane, dpoly)
            if not dst_ids:
                continue

            # Origins (objects/npcs may have origin rect & plane)
            src_ids: List[Optional[int]] = [None]
            if table_name in ("object_nodes","npc_nodes") and oplane is not None and ominx is not None:
                orect = clamp_rect(ominx, omaxx, ominy, omaxy)
                if orect is not None and oplane == plane:
                    opoly = rect_to_poly(*orect)
                    sids = find_cells_intersecting_rect(cur, plane, opoly)
                    if sids:
                        src_ids = sids

            meta = {"dest_rect":[dminx,dmaxx,dminy,dmaxy,dplane]}
            if table_name in ("object_nodes","npc_nodes") and oplane is not None and ominx is not None:
                meta["orig_rect"] = [ominx,omaxx,ominy,omaxy,oplane]

            for dst_id in dst_ids:
                for src_id in src_ids:
                    add_offmesh(specs["key"], table_name, node_id, req_id, cost,
                                plane if src_id is not None else None, plane,
                                src_id, dst_id, meta)

    out_conn.commit()


# -------------------------
# Main
# -------------------------

def parse_bbox(s: Optional[str]) -> Optional[Dict[str,int]]:
    if not s:
        return None
    # Format: "plane=0,xmin=3000,xmax=3400,ymin=3300,ymax=3600"
    parts = [p.strip() for p in s.split(",") if p.strip()]
    out = {}
    for p in parts:
        if "=" in p:
            k,v = p.split("=",1)
            try:
                out[k.strip()] = int(v.strip())
            except ValueError:
                pass
    return out


def main():
    ap = argparse.ArgumentParser(description="Build a polygonal navmesh + overlays into a separate SQLite DB.")
    ap.add_argument("--input", required=True, help="Path to worldReachableTiles.db")
    ap.add_argument("--output", required=True, help="Path to output navmesh.db (created/overwritten)")
    ap.add_argument("--mode", choices=["polys"], default="polys",
                    help="polys: merge tiles into polygons (default)")
    ap.add_argument("--triangulate", action="store_true",
                    help="Triangulate polygons into triangles and build portal adjacency (optional)")
    ap.add_argument("--bbox", type=str, default=None,
                    help='Optional crop: "plane=0,xmin=...,xmax=...,ymin=...,ymax=..."')
    args = ap.parse_args()

    bbox = parse_bbox(args.bbox)

    # Open DBs
    in_conn = sqlite3.connect(args.input)
    in_conn.row_factory = sqlite3.Row
    out_conn = ensure_output_db(args.output)

    planes = fetch_planes(in_conn, bbox)
    if not planes:
        print("No planes found in tiles table.", file=sys.stderr)
        sys.exit(2)

    # Store meta
    out_conn.execute("INSERT OR REPLACE INTO meta(key,value) VALUES(?,?)", ("source_db", args.input))
    out_conn.execute("INSERT OR REPLACE INTO meta(key,value) VALUES(?,?)", ("mode", args.mode))
    out_conn.execute("INSERT OR REPLACE INTO meta(key,value) VALUES(?,?)", ("triangulate", str(bool(args.triangulate))))

    for plane in planes:
        if bbox and "plane" in bbox and plane != int(bbox["plane"]):
            continue
        build_navmesh_for_plane(in_conn, out_conn, plane, args.mode, args.triangulate, bbox)

    print("Done.")


if __name__ == "__main__":
    main()
