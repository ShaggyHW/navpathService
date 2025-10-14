
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
    from shapely.ops import unary_union, triangulate, split, nearest_points
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

DISTINCT_PLANES_SQL = "SELECT DISTINCT plane FROM tiles ORDER BY plane"

NODE_TABLES = {
    "object_nodes": {
        "key": "object",
        "has_origin": True,
        "cols": ("id","match_type","object_id","object_name","action",
                 "dest_min_x","dest_max_x","dest_min_y","dest_max_y","dest_plane",
                 "orig_min_x","orig_max_x","orig_min_y","orig_max_y","orig_plane",
                 "search_radius","cost","next_node_type","next_node_id","requirement_id"),
        "sql": """
            SELECT id, match_type, object_id, object_name, action,
                   dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane,
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

MAX_POLY_EXTENT = 64
HOLE_SPLIT_MIN_RADIUS = 15

def is_tile_walkable(allowed: Optional[str], blocked: Optional[str]) -> bool:
    """
    Conservative walkability: a tile is walkable if it has *any* allowed directions token.
    If allowed is NULL/empty, it's considered non-walkable.
    """
    if not allowed:
        return False
    tokens = {t.strip().upper() for t in allowed.split(",") if t.strip()}
    return len(tokens & DIR_TOKENS) > 0


def _parse_dir_set(s: Optional[str]) -> set:
    if not s:
        return set()
    return {t.strip().upper() for t in s.split(",") if t.strip()}


def _tile_allows_dir(allowed_set: set, blocked_set: set, base: str) -> bool:
    b = base.upper()
    if b == "N":
        syn = {"N", "NORTH"}
    elif b == "E":
        syn = {"E", "EAST"}
    elif b == "S":
        syn = {"S", "SOUTH"}
    elif b == "W":
        syn = {"W", "WEST"}
    else:
        syn = {b}
    if len(syn & allowed_set) == 0:
        return False
    if len(syn & blocked_set) > 0:
        return False
    return True


def _fetch_tile_dirs(conn: sqlite3.Connection, plane: int, bbox: Optional[Dict[str,int]]) -> Dict[Tuple[int,int], Tuple[set, set]]:
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
    out: Dict[Tuple[int,int], Tuple[set, set]] = {}
    for x, y, plane_v, allow, block in conn.execute(sql, params):
        try:
            xx = int(x); yy = int(y)
        except Exception:
            continue
        a = _parse_dir_set(allow)
        b = _parse_dir_set(block)
        if len(a) == 0:
            continue
        out[(xx,yy)] = (a, b)
    return out


def _build_passable_edge_union(conn: sqlite3.Connection, plane: int, bbox: Optional[Dict[str,int]]):
    tile_dirs = _fetch_tile_dirs(conn, plane, bbox)
    segs: List[LineString] = []
    for (x, y), (a_set, b_set) in tile_dirs.items():
        nx = (x + 1, y)
        if nx in tile_dirs:
            a2, b2 = tile_dirs[nx]
            if _tile_allows_dir(a_set, b_set, "E") and _tile_allows_dir(a2, b2, "W"):
                segs.append(LineString([(x + 1, y), (x + 1, y + 1)]))
        ny = (x, y + 1)
        if ny in tile_dirs:
            a2, b2 = tile_dirs[ny]
            if _tile_allows_dir(a_set, b_set, "N") and _tile_allows_dir(a2, b2, "S"):
                segs.append(LineString([(x, y + 1), (x + 1, y + 1)]))
    if not segs:
        return None
    try:
        return unary_union(segs)
    except Exception:
        return None


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


def split_polygon_holes(poly: Polygon, min_radius: float) -> List[Polygon]:
    interiors = list(getattr(poly, "interiors", []) or [])
    if not interiors:
        return [poly]
    lines = []
    ext = poly.exterior
    for ring in interiors:
        try:
            hole_area = Polygon(ring).area
            if hole_area <= 0:
                continue
            radius = math.sqrt(hole_area / math.pi)
            if radius <= min_radius:
                continue
            p_ext, p_hole = nearest_points(ext, ring)
            line = LineString([p_ext, p_hole])
            lines.append(line)
        except Exception:
            continue
    if not lines:
        return [poly]
    splitter = unary_union(lines) if len(lines) > 1 else lines[0]
    try:
        res = split(poly, splitter)
        geoms = list(getattr(res, "geoms", [res]))
    except Exception:
        try:
            slit = splitter.buffer(1e-6, cap_style=2)
            diffed = poly.difference(slit)
            geoms = [g for g in getattr(diffed, "geoms", [diffed])]
        except Exception:
            geoms = [poly]
    out: List[Polygon] = []
    for g in geoms:
        if isinstance(g, Polygon) and not g.is_empty and g.area > 1e-6:
            out.append(g)
    return out or [poly]


def split_holes_in_polys(polys: Iterable[Polygon], min_radius: float) -> List[Polygon]:
    out: List[Polygon] = []
    for p in polys:
        out.extend(split_polygon_holes(p, min_radius))
    return out


def split_polygon_by_extent(poly: Polygon, max_extent: int) -> List[Polygon]:
    minx, miny, maxx, maxy = poly.bounds
    if (maxx - minx) <= max_extent and (maxy - miny) <= max_extent:
        return [poly]
    gx_min = math.floor(minx / max_extent) * max_extent
    gy_min = math.floor(miny / max_extent) * max_extent
    gx_max = math.ceil(maxx / max_extent) * max_extent
    gy_max = math.ceil(maxy / max_extent) * max_extent
    pieces: List[Polygon] = []
    for x0 in range(gx_min, gx_max, max_extent):
        x1 = min(x0 + max_extent, gx_max)
        for y0 in range(gy_min, gy_max, max_extent):
            y1 = min(y0 + max_extent, gy_max)
            cell = box(x0, y0, x1, y1)
            clipped = poly.intersection(cell)
            if clipped.is_empty:
                continue
            if isinstance(clipped, Polygon):
                if clipped.area > 1e-6:
                    pieces.append(clipped)
            else:
                for g in getattr(clipped, "geoms", []):
                    if isinstance(g, Polygon) and not g.is_empty and g.area > 1e-6:
                        pieces.append(g)
    return pieces or [poly]


def enforce_polygon_extent(polys: Iterable[Polygon], max_extent: int) -> List[Polygon]:
    out: List[Polygon] = []
    for poly in polys:
        out.extend(split_polygon_by_extent(poly, max_extent))
    return out


def _grid_sort_key(geom: Polygon, extent: int) -> Tuple[int, int, float, float]:
    minx, miny, _, _ = geom.bounds
    gx = math.floor(minx / extent)
    gy = math.floor(miny / extent)
    return (gy, gx, float(miny), float(minx))


def tiles_to_rectangles(tiles: Iterable[Tuple[int,int]], max_w: int, max_h: int) -> List[Polygon]:
    """
    Greedy maximal rectangles over contiguous tiles, with caps on width/height.
    Produces convex cells and prevents 'mega-polygons'.
    """
    occ = set(tiles)
    visited = set()
    rects: List[Polygon] = []
    for (x0,y0) in sorted(occ, key=lambda p: (p[1], p[0])):
        if (x0,y0) in visited:
            continue
        # grow right
        x1 = x0
        while (x1+1, y0) in occ and (x1+1, y0) not in visited and (x1+1 - x0) < max_w:
            x1 += 1
        # grow down while full rows cover [x0..x1]
        y1 = y0
        while (y1 + 1 - y0) < max_h:
            ny = y1 + 1
            if all((x, ny) in occ and (x, ny) not in visited for x in range(x0, x1+1)):
                y1 = ny
            else:
                break
        for yy in range(y0, y1+1):
            for xx in range(x0, x1+1):
                visited.add((xx, yy))
        rects.append(box(x0, y0, x1+1, y1+1))
    return rects


def triangles_from_polygon(poly: Polygon) -> List[Polygon]:
    """
    Triangulate polygon area; keep only triangles whose centroid lies within the polygon.
    """
    tris = triangulate(poly)
    return [t for t in tris if t.centroid.within(poly)]


def bbox_of_geom(g) -> Tuple[float,float,float,float]:
    minx, miny, maxx, maxy = g.bounds
    return (float(minx), float(miny), float(maxx), float(maxy))


def _door_barrier_segments_for_plane(conn: sqlite3.Connection, plane: int) -> List[LineString]:
    segs: List[LineString] = []
    try:
        rows = conn.execute(NODE_TABLES["door_nodes"]["sql"])
    except Exception:
        return segs
    for (
        _node_id, _direction, _real_open, _real_closed,
        _lox, _loy, _lop, _lcx, _lcy, _lcp,
        ix, iy, ip, ox, oy, op, _open_action, _cost, _next_t, _next_i, _req_id
    ) in rows:
        try:
            ix = int(ix); iy = int(iy); ip = int(ip)
            ox = int(ox); oy = int(oy); op = int(op)
        except Exception:
            continue
        if ip != plane or op != plane:
            continue
        dx = ox - ix
        dy = oy - iy
        if dx == 1 and dy == 0:
            segs.append(LineString([(ix + 1, iy), (ix + 1, iy + 1)]))
        elif dx == -1 and dy == 0:
            segs.append(LineString([(ix, iy), (ix, iy + 1)]))
        elif dy == 1 and dx == 0:
            segs.append(LineString([(ix, iy + 1), (ix + 1, iy + 1)]))
        elif dy == -1 and dx == 0:
            segs.append(LineString([(ix, iy), (ix + 1, iy)]))
        else:
            continue
    return segs


def _apply_barriers_to_polys(polys: Iterable[Polygon], barrier_segs: List[LineString], eps: float = 1e-3) -> List[Polygon]:
    polys = list(polys)
    if not polys or not barrier_segs:
        return polys
    barrier_geom = unary_union(barrier_segs)
    try:
        slit = barrier_geom.buffer(eps, cap_style=2)
    except Exception:
        return polys
    out: List[Polygon] = []
    for poly in polys:
        try:
            clipped = poly.difference(slit)
        except Exception:
            out.append(poly)
            continue
        if isinstance(clipped, Polygon):
            if clipped.area > 1e-6:
                out.append(clipped)
        else:
            for g in getattr(clipped, "geoms", []):
                if isinstance(g, Polygon) and not g.is_empty and g.area > 1e-6:
                    out.append(g)
    return out


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

CREATE TABLE IF NOT EXISTS requirements(
  id INTEGER PRIMARY KEY,
  metaInfo TEXT,
  key TEXT,
  value INTEGER,
  comparison TEXT
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
    # Commit DDL so we start clean (Python sqlite3 wraps DDL in a txn)
    out.commit()
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


def build_adjacency(cells: Dict[int, Polygon], plane: int, cur: sqlite3.Cursor, passable_union=None):
    # Spatial index over cell bboxes
    sidx = SimpleRTree()
    for cid, geom in cells.items():
        sidx.insert(cid, bbox_of_geom(geom))
    EPS = 0.0000001
    
    # For each cell, find neighbors with boundary overlap (LineString segment)
    # To avoid O(n^2) duplicates, only connect cid < nid
    for cid, geom in cells.items():
        print(f"Processing cell {cid}/{len(cells)}")
        bb = bbox_of_geom(geom)
        for nid in sidx.intersects(bb):
            if nid <= cid:
                continue
            ng = cells[nid]
            if geom.distance(ng) > EPS:
                continue
            inter = geom.intersection(ng)
            if inter.is_empty:
                continue
            # intersection could be MultiLineString or LineString
            lines = []
            if isinstance(inter, LineString):
                if inter.length > EPS:
                    lines.append(inter)
            else:
                for g in getattr(inter, "geoms", []):
                    if isinstance(g, LineString) and g.length > EPS:
                        lines.append(g)
            for g in lines:
                p = g.interpolate(0.5, normalized=True)
                mx, my = p.coords[0]
                on_a = abs(geom.boundary.distance(Point(mx, my))) < EPS
                on_b = abs(ng.boundary.distance(Point(mx, my))) < EPS
                if not (on_a and on_b):
                    continue
                g_use = g
                if passable_union is not None:
                    try:
                        g_clip = g.intersection(passable_union)
                    except Exception:
                        g_clip = g
                    g_use = g_clip
                pieces = []
                if isinstance(g_use, LineString):
                    if g_use.length > EPS:
                        pieces.append(g_use)
                else:
                    for gg in getattr(g_use, "geoms", []):
                        if isinstance(gg, LineString) and gg.length > EPS:
                            pieces.append(gg)
                for seg in pieces:
                    x1, y1 = seg.coords[0]
                    x2, y2 = seg.coords[-1]
                    insert_portal(cur, plane, cid, nid, x1, y1, x2, y2)
                    insert_portal(cur, plane, nid, cid, x1, y1, x2, y2)


def find_cell_containing_point(cur: sqlite3.Cursor, plane: int, pt: Point) -> Optional[int]:
    x = float(pt.x); y = float(pt.y)
    # RTree coarse filter
    conn = cur.connection
    for (cid,) in cur.execute(
        """
        SELECT r.id
        FROM rtree_cells AS r
        JOIN cells AS c ON c.id = r.id
        WHERE c.plane = ?
          AND r.minx <= ? AND r.maxx >= ?
          AND r.miny <= ? AND r.maxy >= ?
        """,
        (plane, x, x, y, y),
    ):
        (wkb_bytes,) = conn.execute("SELECT wkb FROM cells WHERE id=?", (cid,)).fetchone()
        geom = wkb.loads(wkb_bytes)
        if geom.contains(pt):
            return cid
    return None


def find_cells_intersecting_rect(cur: sqlite3.Cursor, plane: int, rect_poly: Polygon) -> List[int]:
    minx, miny, maxx, maxy = bbox_of_geom(rect_poly)
    hits = []
    conn = cur.connection
    for (cid,) in cur.execute(
        """
        SELECT r.id
        FROM rtree_cells AS r
        JOIN cells AS c ON c.id = r.id
        WHERE c.plane = ?
          AND r.minx <= ? AND r.maxx >= ?
          AND r.miny <= ? AND r.maxy >= ?
        """,
        (plane, maxx, minx, maxy, miny),
    ):
        (wkb_bytes,) = conn.execute("SELECT wkb FROM cells WHERE id=?", (cid,)).fetchone()
        geom = wkb.loads(wkb_bytes)
        if geom.intersects(rect_poly):
            hits.append(cid)
    return hits


def build_navmesh_for_plane(in_conn: sqlite3.Connection, out_conn: sqlite3.Connection,
                            plane: int, mode: str, triangulate_flag: bool,
                            bbox: Optional[Dict[str,int]],
                            cell_shape: str, rect_max_w: int, rect_max_h: int):
    print(f"[plane {plane}] Fetching tiles ...")
    tiles = fetch_tiles(in_conn, plane, bbox)
    if not tiles:
        print(f"[plane {plane}] No walkable tiles.")
        return

    if cell_shape == "rects" and not triangulate_flag:
        print(f"[plane {plane}] Packing tiles into rectangles (max {rect_max_w}x{rect_max_h}) ...")
        polys = tiles_to_rectangles(tiles, rect_max_w, rect_max_h)
        polys = split_holes_in_polys(polys, HOLE_SPLIT_MIN_RADIUS)
        polys = enforce_polygon_extent(polys, MAX_POLY_EXTENT)
    else:
        print(f"[plane {plane}] Merging tiles into polygons ...")
        polys = tiles_to_polygons(tiles)
        for _ in range(3):
            has_big_hole = False
            for p in polys:
                if isinstance(p, Polygon) and getattr(p, "interiors", None):
                    for ring in p.interiors:
                        try:
                            a = Polygon(ring).area
                            if a > 0 and math.sqrt(a / math.pi) > HOLE_SPLIT_MIN_RADIUS:
                                has_big_hole = True
                                break
                        except Exception:
                            continue
                if has_big_hole:
                    break
            if not has_big_hole:
                break
            polys = split_holes_in_polys(polys, HOLE_SPLIT_MIN_RADIUS)
        polys = enforce_polygon_extent(polys, MAX_POLY_EXTENT)

    # Prevent cells merging across door edges; split polys along door barriers
    door_segs = _door_barrier_segments_for_plane(in_conn, plane)
    if door_segs:
        before = len(polys)
        polys = _apply_barriers_to_polys(polys, door_segs)
        after = len(polys)
        if after != before:
            print(f"[plane {plane}] Door barriers applied: {before} -> {after} cells before triangulation")

    cur = out_conn.cursor()

    cell_geoms: Dict[int, Polygon] = {}  # id -> geometry
    passable_union = _build_passable_edge_union(in_conn, plane, bbox)

    if mode == "polys" and not triangulate_flag:
        polys = sorted(polys, key=lambda g: _grid_sort_key(g, MAX_POLY_EXTENT))
        for poly in polys:
            cid = insert_cell(cur, plane, "polygon", poly)
            cell_geoms[cid] = poly
        print(f"[plane {plane}] Building polygon adjacency (portals) ...")
        build_adjacency(cell_geoms, plane, cur, passable_union=passable_union)
    else:
        # Triangulate each polygon; keep triangles within polygon only.
        print(f"[plane {plane}] Triangulating polygons into triangles ...")
        for poly in sorted(polys, key=lambda g: _grid_sort_key(g, MAX_POLY_EXTENT)):
            tris = triangles_from_polygon(poly)
            tris = sorted(tris, key=lambda g: _grid_sort_key(g, MAX_POLY_EXTENT))
            for t in tris:
                cid = insert_cell(cur, plane, "triangle", t)
                cell_geoms[cid] = t
        print(f"[plane {plane}] Building triangle adjacency (portals) ...")
        build_adjacency(cell_geoms, plane, cur, passable_union=None)

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


def _gather_non_heads(conn: sqlite3.Connection) -> Dict[str, set]:
    idx: Dict[str, set] = {}
    for key, table in (
        ("door", "door_nodes"),
        ("lodestone", "lodestone_nodes"),
        ("object", "object_nodes"),
        ("ifslot", "ifslot_nodes"),
        ("npc", "npc_nodes"),
        ("item", "item_nodes"),
    ):
        try:
            for t, i in conn.execute(
                f"SELECT next_node_type, next_node_id FROM {table} WHERE next_node_type IS NOT NULL AND next_node_id IS NOT NULL"
            ):
                if t is None or i is None:
                    continue
                tt = str(t).strip().lower()
                try:
                    ii = int(i)
                except Exception:
                    continue
                s = idx.setdefault(tt, set())
                s.add(ii)
        except Exception:
            continue
    return idx


def _fetch_next(conn: sqlite3.Connection, kind: Optional[str], node_id: Optional[int]) -> Tuple[Optional[str], Optional[int]]:
    if not kind or node_id is None:
        return None, None
    t = str(kind).strip().lower()
    table = None
    if t == "door":
        table = "door_nodes"
    elif t == "lodestone":
        table = "lodestone_nodes"
    elif t == "object":
        table = "object_nodes"
    elif t == "ifslot":
        table = "ifslot_nodes"
    elif t == "npc":
        table = "npc_nodes"
    elif t == "item":
        table = "item_nodes"
    if not table:
        return None, None
    try:
        row = conn.execute(f"SELECT next_node_type, next_node_id FROM {table} WHERE id = ?", (int(node_id),)).fetchone()
    except Exception:
        row = None
    if not row:
        return None, None
    nxt_t, nxt_i = row
    if nxt_t is None or nxt_i is None:
        return None, None
    try:
        return str(nxt_t).strip().lower(), int(nxt_i)
    except Exception:
        return None, None


def _fetch_node_meta(conn: sqlite3.Connection, kind: str, node_id: int) -> Dict:
    t = str(kind).strip().lower()
    if t == "lodestone":
        row = conn.execute("SELECT id,lodestone,dest_x,dest_y,dest_plane,cost,next_node_type,next_node_id,requirement_id FROM lodestone_nodes WHERE id=?", (node_id,)).fetchone()
        if not row:
            return {}
        _, lodename, dx, dy, dplane, cost, next_node_type, next_node_id, _ = row
        return {"lodestone": lodename, "dest_point": [dx,dy,dplane], "next_node_type": next_node_type, "next_node_id": next_node_id}
    if t == "door":
        row = conn.execute("SELECT id,direction,real_id_open,real_id_closed,location_open_x,location_open_y,location_open_plane,location_closed_x,location_closed_y,location_closed_plane,tile_inside_x,tile_inside_y,tile_inside_plane,tile_outside_x,tile_outside_y,tile_outside_plane,open_action,cost,next_node_type,next_node_id,requirement_id FROM door_nodes WHERE id=?", (node_id,)).fetchone()
        if not row:
            return {}
        (_, direction, real_open, real_closed, lox, loy, lop, lcx, lcy, lcp, ix, iy, ip, ox, oy, op, open_action, cost, next_node_type, next_node_id, _) = row
        meta = {"inside":[ix,iy,ip], "outside":[ox,oy,op], "open_action": open_action, "real_id_open": real_open, "real_id_closed": real_closed, "direction": direction}
        return meta
    if t == "object":
        row = conn.execute("SELECT id,match_type,object_id,object_name,action,dest_min_x,dest_max_x,dest_min_y,dest_max_y,dest_plane,orig_min_x,orig_max_x,orig_min_y,orig_max_y,orig_plane,search_radius,cost,next_node_type,next_node_id,requirement_id FROM object_nodes WHERE id=?", (node_id,)).fetchone()
        if not row:
            return {}
        ( _id, match_type, object_id, object_name, action, dminx, dmaxx, dminy, dmaxy, dplane, ominx, omaxx, ominy, omaxy, oplane, _sr, _cost, next_node_type, next_node_id, _req) = row
        meta = {"action": action, "object_id": object_id, "object_name": object_name, "match_type": match_type, "dest_rect": [dminx,dmaxx,dminy,dmaxy,dplane], "next_node_type": next_node_type, "next_node_id": next_node_id}
        if oplane is not None and ominx is not None:
            meta["orig_rect"] = [ominx,omaxx,ominy,omaxy,oplane]
        return meta
    if t == "npc":
        row = conn.execute("SELECT id,match_type,npc_id,npc_name,action,dest_min_x,dest_max_x,dest_min_y,dest_max_y,dest_plane,search_radius,cost,orig_min_x,orig_max_x,orig_min_y,orig_max_y,orig_plane,next_node_type,next_node_id,requirement_id FROM npc_nodes WHERE id=?", (node_id,)).fetchone()
        if not row:
            return {}
        (_id, match_type, npc_id, npc_name, action, dminx, dmaxx, dminy, dmaxy, dplane, _sr, _cost, ominx, omaxx, ominy, omaxy, oplane, next_node_type, next_node_id, _req) = row
        meta = {"action": action, "npc_id": npc_id, "npc_name": npc_name, "match_type": match_type, "dest_rect": [dminx,dmaxx,dminy,dmaxy,dplane], "next_node_type": next_node_type, "next_node_id": next_node_id}
        if oplane is not None and ominx is not None:
            meta["orig_rect"] = [ominx,omaxx,ominy,omaxy,oplane]
        return meta
    if t == "ifslot":
        row = conn.execute("SELECT id,interface_id,component_id,slot_id,click_id,dest_min_x,dest_max_x,dest_min_y,dest_max_y,dest_plane,cost,next_node_type,next_node_id,requirement_id FROM ifslot_nodes WHERE id=?", (node_id,)).fetchone()
        if not row:
            return {}
        (_id, interface_id, component_id, slot_id, click_id, dminx, dmaxx, dminy, dmaxy, dplane, _cost, next_node_type, next_node_id, _req) = row
        return {"interface_id": interface_id, "component_id": component_id, "slot_id": slot_id, "click_id": click_id, "dest_rect": [dminx,dmaxx,dminy,dmaxy,dplane], "next_node_type": next_node_type, "next_node_id": next_node_id}
    if t == "item":
        row = conn.execute("SELECT id,item_id,action,dest_min_x,dest_max_x,dest_min_y,dest_max_y,dest_plane,cost,next_node_type,next_node_id,requirement_id FROM item_nodes WHERE id=?", (node_id,)).fetchone()
        if not row:
            return {}
        (_id, item_id, action, dminx, dmaxx, dminy, dmaxy, dplane, _cost, next_node_type, next_node_id, _req) = row
        return {"item_id": item_id, "action": action, "dest_rect": [dminx,dmaxx,dminy,dmaxy,dplane], "next_node_type": next_node_type, "next_node_id": next_node_id}
    return {}


def _build_child_chain(conn: sqlite3.Connection, start_kind: Optional[str], start_id: Optional[int], max_depth: int = 32) -> List[Dict[str, int]]:
    out: List[Dict[str, int]] = []
    seen: set = set()
    kind = start_kind.strip().lower() if isinstance(start_kind, str) else None
    try:
        cur_id = int(start_id) if start_id is not None else None
    except Exception:
        cur_id = None
    depth = 0
    while kind and cur_id is not None and depth < max_depth:
        key = (kind, cur_id)
        if key in seen:
            break
        seen.add(key)
        meta = _fetch_node_meta(conn, kind, cur_id)
        out.append({"type": kind, "id": cur_id, "meta": meta})
        nxt_kind, nxt_id = _fetch_next(conn, kind, cur_id)
        kind, cur_id = nxt_kind, nxt_id
        depth += 1
    return out


def build_overlays_for_plane(in_conn: sqlite3.Connection, out_conn: sqlite3.Connection, plane: int):
    cur = out_conn.cursor()
    req_ids: set = set()
    non_heads = _gather_non_heads(in_conn)
    def is_non_head(kind: str, node_id: int) -> bool:
        return node_id in non_heads.get(kind, set())

    # Helper: insert offmesh row
    def add_offmesh(link_type: str, node_table: str, node_id: int, req_id: Optional[int],
                    cost: Optional[float], plane_from: Optional[int], plane_to: int,
                    src_cell_id: Optional[int], dst_cell_id: Optional[int], meta: Dict):
        # Safety: require a concrete destination cell for this run
        if dst_cell_id is None:
            return
        cur.execute("""
            INSERT INTO offmesh_links(link_type,node_table,node_id,requirement_id,cost,
                                      plane_from,plane_to,src_cell_id,dst_cell_id,meta_json)
            VALUES(?,?,?,?,?,?,?,?,?,?)
        """, (link_type, node_table, node_id, req_id, cost,
              plane_from, plane_to, src_cell_id, dst_cell_id, json.dumps(meta, separators=(",",":"))))
        if req_id is not None:
            req_ids.add(int(req_id))

    # Lodestones (point destination)
    rows = in_conn.execute(NODE_TABLES["lodestone_nodes"]["sql"])
    for node_id, lodename, dx, dy, dplane, cost, next_node_type, next_node_id, req_id in rows:
        if is_non_head("lodestone", int(node_id)):
            continue
        if dplane != plane:
            continue
        pt = Point(float(dx)+0.5, float(dy)+0.5)
        dst_id = find_cell_containing_point(cur, plane, pt)
        if dst_id is None:
            continue
        cc = _build_child_chain(in_conn, next_node_type, next_node_id)
        add_offmesh(
            "lodestone","lodestone_nodes",node_id,req_id,cost,None,plane,None,dst_id,
            {
                "target_lodestone": lodename,
                "lodestone": lodename,
                "dest_point": [dx,dy,dplane],
                "next_node_type": next_node_type,
                "next_node_id": next_node_id,
                "child_chain": cc,
            }
        )

    # Doors (connect inside/outside tiles)
    rows = in_conn.execute(NODE_TABLES["door_nodes"]["sql"])
    for (node_id, direction, real_open, real_closed,
         lox, loy, lop, lcx, lcy, lcp,
         ix, iy, ip, ox, oy, op, open_action, cost, next_node_type, next_node_id, req_id) in rows:
        if is_non_head("door", int(node_id)):
            continue
        # Map tiles to cells on their respective planes
        a_id = None
        b_id = None
        if ip == plane:
            a_pt = Point(float(ix)+0.5, float(iy)+0.5)
            a_id = find_cell_containing_point(cur, plane, a_pt)
        if op == plane:
            b_pt = Point(float(ox)+0.5, float(oy)+0.5)
            b_id = find_cell_containing_point(cur, plane, b_pt)

        # Two links: inside→outside on IP, outside→inside on OP
        if a_id is not None:
            cc = _build_child_chain(in_conn, next_node_type, next_node_id)
            db_row = {
                "id": node_id,
                "direction": direction,
                "real_id_open": real_open,
                "real_id_closed": real_closed,
                "location_open_x": lox,
                "location_open_y": loy,
                "location_open_plane": lop,
                "location_closed_x": lcx,
                "location_closed_y": lcy,
                "location_closed_plane": lcp,
                "tile_inside_x": ix,
                "tile_inside_y": iy,
                "tile_inside_plane": ip,
                "tile_outside_x": ox,
                "tile_outside_y": oy,
                "tile_outside_plane": op,
                "open_action": open_action,
                "cost": cost,
                "next_node_type": next_node_type,
                "next_node_id": next_node_id,
                "requirement_id": req_id,
                "child_chain": cc,
            }
            meta_a = {
                "inside": [ix,iy,ip],
                "outside": [ox,oy,op],
                "open_action": open_action,
                "real_id_open": real_open,
                "real_id_closed": real_closed,
                "direction": direction,
                "location_open_x": lox,
                "location_open_y": loy,
                "location_open_plane": lop,
                "location_closed_x": lcx,
                "location_closed_y": lcy,
                "location_closed_plane": lcp,
                "child_chain": cc,
                "db_row": db_row,
            }
            add_offmesh("door","door_nodes",node_id,req_id,cost,ip,op,a_id,b_id if b_id is not None else None, meta_a)
        if b_id is not None:
            cc = _build_child_chain(in_conn, next_node_type, next_node_id)
            db_row = {
                "id": node_id,
                "direction": direction,
                "real_id_open": real_open,
                "real_id_closed": real_closed,
                "location_open_x": lox,
                "location_open_y": loy,
                "location_open_plane": lop,
                "location_closed_x": lcx,
                "location_closed_y": lcy,
                "location_closed_plane": lcp,
                "tile_inside_x": ix,
                "tile_inside_y": iy,
                "tile_inside_plane": ip,
                "tile_outside_x": ox,
                "tile_outside_y": oy,
                "tile_outside_plane": op,
                "open_action": open_action,
                "cost": cost,
                "next_node_type": next_node_type,
                "next_node_id": next_node_id,
                "requirement_id": req_id,
                "child_chain": cc,
            }
            meta_b = {
                "inside": [ix,iy,ip],
                "outside": [ox,oy,op],
                "open_action": open_action,
                "real_id_open": real_open,
                "real_id_closed": real_closed,
                "direction": direction,
                "location_open_x": lox,
                "location_open_y": loy,
                "location_open_plane": lop,
                "location_closed_x": lcx,
                "location_closed_y": lcy,
                "location_closed_plane": lcp,
                "child_chain": cc,
                "db_row": db_row,
            }
            add_offmesh("door","door_nodes",node_id,req_id,cost,op,ip,b_id,a_id if a_id is not None else None, meta_b)

        # Same-plane: add both directions when both endpoints are in this plane
        if ip == op == plane and a_id is not None and b_id is not None:
            cc = _build_child_chain(in_conn, next_node_type, next_node_id)
            db_row = {
                "id": node_id,
                "direction": direction,
                "real_id_open": real_open,
                "real_id_closed": real_closed,
                "location_open_x": lox,
                "location_open_y": loy,
                "location_open_plane": lop,
                "location_closed_x": lcx,
                "location_closed_y": lcy,
                "location_closed_plane": lcp,
                "tile_inside_x": ix,
                "tile_inside_y": iy,
                "tile_inside_plane": ip,
                "tile_outside_x": ox,
                "tile_outside_y": oy,
                "tile_outside_plane": op,
                "open_action": open_action,
                "cost": cost,
                "next_node_type": next_node_type,
                "next_node_id": next_node_id,
                "requirement_id": req_id,
                "child_chain": cc,
            }
            meta = {
                "inside": [ix,iy,ip],
                "outside": [ox,oy,op],
                "open_action": open_action,
                "real_id_open": real_open,
                "real_id_closed": real_closed,
                "direction": direction,
                "location_open_x": lox,
                "location_open_y": loy,
                "location_open_plane": lop,
                "location_closed_x": lcx,
                "location_closed_y": lcy,
                "location_closed_plane": lcp,
                "child_chain": cc,
                "db_row": db_row,
            }
            add_offmesh("door","door_nodes",node_id,req_id,cost,plane,plane,a_id,b_id,meta)
            add_offmesh("door","door_nodes",node_id,req_id,cost,plane,plane,b_id,a_id,meta)
        else:
            # Cross-plane: only add the inbound link that LANDS in this plane (dst known)
            cc = _build_child_chain(in_conn, next_node_type, next_node_id)
            db_row = {
                "id": node_id,
                "direction": direction,
                "real_id_open": real_open,
                "real_id_closed": real_closed,
                "location_open_x": lox,
                "location_open_y": loy,
                "location_open_plane": lop,
                "location_closed_x": lcx,
                "location_closed_y": lcy,
                "location_closed_plane": lcp,
                "tile_inside_x": ix,
                "tile_inside_y": iy,
                "tile_inside_plane": ip,
                "tile_outside_x": ox,
                "tile_outside_y": oy,
                "tile_outside_plane": op,
                "open_action": open_action,
                "cost": cost,
                "next_node_type": next_node_type,
                "next_node_id": next_node_id,
                "requirement_id": req_id,
                "child_chain": cc,
            }
            meta = {
                "inside": [ix,iy,ip],
                "outside": [ox,oy,op],
                "open_action": open_action,
                "real_id_open": real_open,
                "real_id_closed": real_closed,
                "direction": direction,
                "child_chain": cc,
                "db_row": db_row,
            }
            if op == plane and b_id is not None:
                # From ip -> op(this plane), src unknown in this run
                add_offmesh("door","door_nodes",node_id,req_id,cost,ip,op,None,b_id,meta)
            if ip == plane and a_id is not None:
                # From op -> ip(this plane), src unknown in this run
                add_offmesh("door","door_nodes",node_id,req_id,cost,op,ip,None,a_id,meta)

    # Rectangle-destination nodes
    for table_name in ("object_nodes","npc_nodes","ifslot_nodes","item_nodes"):
        specs = NODE_TABLES[table_name]
        rows = in_conn.execute(specs["sql"])
        for row in rows:
            if table_name == "object_nodes":
                (node_id, match_type, object_id, object_name, action,
                 dminx, dmaxx, dminy, dmaxy, dplane,
                 ominx, omaxx, ominy, omaxy, oplane,
                 search_radius, cost, next_node_type, next_node_id, req_id) = row
            elif table_name == "npc_nodes":
                (node_id, match_type, npc_id, npc_name, action,
                 dminx, dmaxx, dminy, dmaxy, dplane,
                 search_radius, cost, ominx, omaxx, ominy, omaxy, oplane,
                 next_node_type, next_node_id, req_id) = row
            elif table_name == "ifslot_nodes":
                (node_id, interface_id, component_id, slot_id, click_id,
                 dminx, dmaxx, dminy, dmaxy, dplane,
                 cost, next_node_type, next_node_id, req_id) = row
                ominx=omaxx=ominy=omaxy=oplane=None
            else:  # item_nodes
                (node_id, item_id, action,
                 dminx, dmaxx, dminy, dmaxy, dplane,
                 cost, next_node_type, next_node_id, req_id) = row
                ominx=omaxx=ominy=omaxy=oplane=None

            if is_non_head(specs["key"], int(node_id)):
                continue

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
            # Per-type enrichment for OG metadata parity
            if table_name == "object_nodes":
                meta.update({
                    "action": action,
                    "object_id": object_id,
                    "object_name": object_name,
                    "match_type": match_type,
                    "next_node_type": next_node_type,
                    "next_node_id": next_node_id,
                })
            elif table_name == "npc_nodes":
                meta.update({
                    "action": action,
                    "npc_id": npc_id,
                    "npc_name": npc_name,
                    "match_type": match_type,
                    "next_node_type": next_node_type,
                    "next_node_id": next_node_id,
                })
            elif table_name == "ifslot_nodes":
                meta.update({
                    "interface_id": interface_id,
                    "component_id": component_id,
                    "slot_id": slot_id,
                    "click_id": click_id,
                    "next_node_type": next_node_type,
                    "next_node_id": next_node_id,
                })
            else:  # item_nodes
                meta.update({
                    "item_id": item_id,
                    "action": action,
                    "next_node_type": next_node_type,
                    "next_node_id": next_node_id,
                })
            cc = _build_child_chain(in_conn, next_node_type, next_node_id)
            meta["child_chain"] = cc

            for dst_id in dst_ids:
                for src_id in src_ids:
                    add_offmesh(specs["key"], table_name, node_id, req_id, cost,
                                plane if src_id is not None else None, plane,
                                src_id, dst_id, meta)

    # Ensure requirements referenced by any added offmesh rows exist in output DB
    copy_requirements(in_conn, out_conn, req_ids)

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



def cell_id_for_tile(out_conn: sqlite3.Connection, plane: int, x: int, y: int) -> Optional[int]:
    cur = out_conn.cursor()
    pt = Point(float(x)+0.5, float(y)+0.5)
    return find_cell_containing_point(cur, plane, pt)


def cell_ids_for_rect(out_conn: sqlite3.Connection, plane: int,
                      xmin: int, xmax: int, ymin: int, ymax: int) -> List[int]:
    cur = out_conn.cursor()
    rect = rect_to_poly(xmin, xmax, ymin, ymax)
    return find_cells_intersecting_rect(cur, plane, rect)


def copy_requirements(in_conn: sqlite3.Connection, out_conn: sqlite3.Connection, req_ids: set):
    """
    Ensure that all requirement rows referenced by offmesh links exist in the output DB.
    Missing rows in the source DB are skipped gracefully.
    """
    if not req_ids:
        return
    cur_out = out_conn.cursor()
    for rid in sorted({int(r) for r in req_ids if r is not None}):
        try:
            row = in_conn.execute(
                "SELECT id, metaInfo, key, value, comparison FROM requirements WHERE id = ?",
                (rid,),
            ).fetchone()
        except Exception:
            row = None
        if row is None:
            continue
        cur_out.execute(
            "INSERT OR IGNORE INTO requirements(id, metaInfo, key, value, comparison) VALUES(?,?,?,?,?)",
            (row[0], row[1], row[2], row[3], row[4]),
        )


def ensure_offmesh(out_conn: sqlite3.Connection, link_type: str, node_table: str, node_id: int,
                   requirement_id: Optional[int], cost: Optional[float],
                   plane_from: Optional[int], plane_to: int,
                   src_cell_id: Optional[int], dst_cell_id: Optional[int], meta: Dict):
    if dst_cell_id is None:
        return
    cur = out_conn.cursor()
    # Check existence
    row = cur.execute("""
        SELECT id FROM offmesh_links
        WHERE link_type=? AND node_table=? AND node_id=?
          AND IFNULL(requirement_id,'')=IFNULL(?, '')
          AND IFNULL(cost,'')=IFNULL(?, '')
          AND IFNULL(plane_from,'')=IFNULL(?, '')
          AND plane_to=?
          AND IFNULL(src_cell_id,'')=IFNULL(?, '')
          AND dst_cell_id=?
    """, (link_type, node_table, node_id, requirement_id, cost, plane_from, plane_to, src_cell_id, dst_cell_id)).fetchone()
    if row is None:
        cur.execute("""
            INSERT INTO offmesh_links(link_type,node_table,node_id,requirement_id,cost,
                                      plane_from,plane_to,src_cell_id,dst_cell_id,meta_json)
            VALUES(?,?,?,?,?,?,?,?,?,?)
        """, (link_type, node_table, node_id, requirement_id, cost,
              plane_from, plane_to, src_cell_id, dst_cell_id, json.dumps(meta, separators=(",",":"))))
        out_conn.commit()


def resolve_crossplane(in_conn: sqlite3.Connection, out_conn: sqlite3.Connection):
    print("[resolve] Resolving cross-plane doors and origin→dest links ...")
    req_ids: set = set()
    non_heads = _gather_non_heads(in_conn)
    def is_non_head(kind: str, node_id: int) -> bool:
        return node_id in non_heads.get(kind, set())

    # Doors: ensure both directions with concrete src/dst ids
    rows = in_conn.execute(NODE_TABLES["door_nodes"]["sql"])
    for (node_id, direction, real_open, real_closed,
         lox, loy, lop, lcx, lcy, lcp,
         ix, iy, ip, ox, oy, op, open_action, cost, next_node_type, next_node_id, req_id) in rows:
        if is_non_head("door", int(node_id)):
            continue
        if req_id is not None:
            req_ids.add(int(req_id))
        a_id = cell_id_for_tile(out_conn, ip, ix, iy)
        b_id = cell_id_for_tile(out_conn, op, ox, oy)
        cc = _build_child_chain(in_conn, next_node_type, next_node_id)
        db_row = {
            "id": node_id,
            "direction": direction,
            "real_id_open": real_open,
            "real_id_closed": real_closed,
            "location_open_x": lox,
            "location_open_y": loy,
            "location_open_plane": lop,
            "location_closed_x": lcx,
            "location_closed_y": lcy,
            "location_closed_plane": lcp,
            "tile_inside_x": ix,
            "tile_inside_y": iy,
            "tile_inside_plane": ip,
            "tile_outside_x": ox,
            "tile_outside_y": oy,
            "tile_outside_plane": op,
            "open_action": open_action,
            "cost": cost,
            "next_node_type": next_node_type,
            "next_node_id": next_node_id,
            "requirement_id": req_id,
            "child_chain": cc,
        }
        meta = {"inside":[ix,iy,ip], "outside":[ox,oy,op], "open_action": open_action,
                "real_id_open": real_open, "real_id_closed": real_closed, "direction": direction,
                "child_chain": cc, "db_row": db_row}
        # Add both directions if endpoints exist
        if a_id is not None and b_id is not None:
            ensure_offmesh(out_conn, "door", "door_nodes", node_id, req_id, cost, ip, op, a_id, b_id, meta)
            ensure_offmesh(out_conn, "door", "door_nodes", node_id, req_id, cost, op, ip, b_id, a_id, meta)

    # Object/NPC: origin plane may differ from dest plane → create origin→dest links
    for table_name in ("object_nodes","npc_nodes"):
        rows = in_conn.execute(NODE_TABLES[table_name]["sql"])
        for row in rows:
            if table_name == "object_nodes":
                (node_id, match_type, object_id, object_name, action,
                 dminx, dmaxx, dminy, dmaxy, dplane,
                 ominx, omaxx, ominy, omaxy, oplane,
                 search_radius, cost, next_node_type, next_node_id, req_id) = row
            else:
                (node_id, match_type, npc_id, npc_name, action,
                 dminx, dmaxx, dminy, dmaxy, dplane,
                 search_radius, cost, ominx, omaxx, ominy, omaxy, oplane,
                 next_node_type, next_node_id, req_id) = row
            key = NODE_TABLES[table_name]["key"]
            if is_non_head(key, int(node_id)):
                continue
            if req_id is not None:
                req_ids.add(int(req_id))
            # Need both planes known
            if None in (ominx, omaxx, ominy, omaxy, oplane, dplane):
                continue
            d_ids = cell_ids_for_rect(out_conn, dplane, int(dminx), int(dmaxx), int(dminy), int(dmaxy))
            o_ids = cell_ids_for_rect(out_conn, oplane, int(ominx), int(omaxx), int(ominy), int(omaxy))
            if not d_ids or not o_ids:
                continue
            cc = _build_child_chain(in_conn, next_node_type, next_node_id)
            meta = {"dest_rect":[dminx,dmaxx,dminy,dmaxy,dplane],
                    "orig_rect":[ominx,omaxx,ominy,omaxy,oplane],
                    "child_chain": cc}
            for src_id in o_ids:
                for dst_id in d_ids:
                    ensure_offmesh(out_conn, NODE_TABLES[table_name]["key"], table_name, node_id,
                                   req_id, cost, oplane, dplane, src_id, dst_id, meta)

    # Copy any referenced requirement rows into the output DB
    copy_requirements(in_conn, out_conn, req_ids)

    print("[resolve] Done.")

def main():
    ap = argparse.ArgumentParser(description="Build a polygonal navmesh + overlays into a separate SQLite DB.")
    ap.add_argument("--input", required=True, help="Path to worldReachableTiles.db")
    ap.add_argument("--output", required=True, help="Path to output navmesh.db (created/overwritten)")
    ap.add_argument("--mode", choices=["polys"], default="polys",
                    help="polys: merge tiles into polygons (default)")
    ap.add_argument("--triangulate", action="store_true",
                    help="Triangulate polygons into triangles and build portal adjacency (optional)")
    ap.add_argument("--cell-shape", choices=["polys","rects"], default="polys",
                    help="Cellization strategy when not triangulating: 'polys' (merged) or 'rects' (maximal rectangles). Default: rects")
    ap.add_argument("--rect-max-w", type=int, default=64, help="Max rectangle width in tiles (rects mode)")
    ap.add_argument("--rect-max-h", type=int, default=64, help="Max rectangle height in tiles (rects mode)")
    ap.add_argument("--bbox", type=str, default=None,
                    help='Optional crop: "plane=0,xmin=...,xmax=...,ymin=...,ymax=..."')
    ap.add_argument("--resolve-crossplane", action="store_true",
                    help="After building cells/overlays, resolve cross-plane doors and origin→dest links using the built meshes for all planes present.")
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
    out_conn.commit()

    for plane in planes:
        if bbox and "plane" in bbox and plane != int(bbox["plane"]):
            continue
        build_navmesh_for_plane(in_conn, out_conn, plane, args.mode, args.triangulate, bbox,
                                args.cell_shape, args.rect_max_w, args.rect_max_h)

    if args.resolve_crossplane:
        resolve_crossplane(in_conn, out_conn)

    print("Done.")


if __name__ == "__main__":
    main()
