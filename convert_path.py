"""Convert a navpath /route response into a Java ``Coordinate[]`` literal.

    python convert_path.py                      # result.json -> results_parsed.json
    python convert_path.py resp.json -o out.txt [--source path] [--snapshot graph.snapshot]

Coordinates come from, in order (``--source auto``): the response's ``geometry``, its
``actions`` (``to``/``from`` points), or its numeric node-id ``path`` resolved through the
snapshot (``SNAPSHOT_PATH`` or ``--snapshot``, default ``graph.snapshot``).
"""
import argparse
import json
import mmap
import os
import struct
from dataclasses import dataclass
from typing import Callable, Dict, Iterable, List, Tuple, Union

Coord = Tuple[int, int, int]


# --- Snapshot layout -------------------------------------------------------------------
# The ONLY place that knows the binary layout (mirror of
# rust/navpath-core/src/snapshot/manifest.rs). The header is magic "NPSS", u32 version,
# then per-version u32 counts and u64 section offsets. A new snapshot version is one new
# LAYOUTS entry: its count/section names (in header order) and its coordinate codec.

SNAPSHOT_MAGIC = b"NPSS"


def _coords_packed_u32(buf, header: "SnapshotHeader", ids: Iterable[int]) -> List[Coord]:
    """v8: one u32 per node in the ``coords`` section, plane<<30 | y<<15 | x."""
    nodes = header.counts["nodes"]
    off = header.section("coords", nodes * 4)
    u32 = struct.Struct("<I")
    out: List[Coord] = []
    for nid in ids:
        i = int(nid)
        if 0 <= i < nodes:  # skip invalid ids gracefully
            key = u32.unpack_from(buf, off + i * 4)[0]
            out.append((key & 0x7FFF, (key >> 15) & 0x7FFF, key >> 30))
    return out


def _gather15(k: int) -> int:
    """Inverse Morton spread: collect the even bits of ``k`` (15 bits)."""
    x = k & 0x15555555
    x = (x | (x >> 1)) & 0x33333333
    x = (x | (x >> 2)) & 0x0F0F0F0F
    x = (x | (x >> 4)) & 0x00FF00FF
    return (x | (x >> 8)) & 0x7FFF


def _coords_morton_u32(buf, header: "SnapshotHeader", ids: Iterable[int]) -> List[Coord]:
    """v9: one u32 per node, plane<<30 | morton(x, y) (x on even bits, y on odd bits)."""
    nodes = header.counts["nodes"]
    off = header.section("coords", nodes * 4)
    u32 = struct.Struct("<I")
    out: List[Coord] = []
    for nid in ids:
        i = int(nid)
        if 0 <= i < nodes:
            key = u32.unpack_from(buf, off + i * 4)[0]
            out.append((_gather15(key), _gather15(key >> 1), key >> 30))
    return out


@dataclass(frozen=True)
class SnapshotLayout:
    counts: Tuple[str, ...]  # u32 counts after magic + version, in header order
    sections: Tuple[str, ...]  # u64 section offsets after the counts, in header order
    coords: Callable[..., List[Coord]]  # (buf, header, ids) -> [(x, y, plane)]


LAYOUTS: Dict[int, SnapshotLayout] = {
    8: SnapshotLayout(
        counts=("nodes", "walk_edges", "macro_edges", "req_tags", "landmarks",
                "fairy_rings", "walk_components"),
        sections=("coords", "walk_offsets", "walk_dst", "walk_diag", "comp",
                  "macro_src", "macro_dst", "macro_w", "macro_kind_first",
                  "macro_id_first", "macro_meta_offs", "macro_meta_lens",
                  "macro_meta_blob", "req_tags", "landmarks", "lm_tab", "fairy_nodes",
                  "fairy_cost_ms", "fairy_meta_offs", "fairy_meta_lens",
                  "fairy_meta_blob"),
        coords=_coords_packed_u32,
    ),
    # v9: same counts and sections (the ALT section may be packed; this script never
    # reads it), Morton coordinate keys.
    9: SnapshotLayout(
        counts=("nodes", "walk_edges", "macro_edges", "req_tags", "landmarks",
                "fairy_rings", "walk_components"),
        sections=("coords", "walk_offsets", "walk_dst", "walk_diag", "comp",
                  "macro_src", "macro_dst", "macro_w", "macro_kind_first",
                  "macro_id_first", "macro_meta_offs", "macro_meta_lens",
                  "macro_meta_blob", "req_tags", "landmarks", "lm_tab", "fairy_nodes",
                  "fairy_cost_ms", "fairy_meta_offs", "fairy_meta_lens",
                  "fairy_meta_blob"),
        coords=_coords_morton_u32,
    ),
}


@dataclass(frozen=True)
class SnapshotHeader:
    version: int
    counts: Dict[str, int]
    offsets: Dict[str, int]
    layout: SnapshotLayout
    file_size: int

    def section(self, name: str, nbytes: int) -> int:
        """Offset of section ``name``, checked to hold ``nbytes`` within the file."""
        off = self.offsets[name]
        if off > self.file_size or self.file_size - off < nbytes:
            raise ValueError(f"snapshot section {name} out of bounds")
        return off


def read_snapshot_header(buf) -> SnapshotHeader:
    if len(buf) < 8 or bytes(buf[0:4]) != SNAPSHOT_MAGIC:
        raise ValueError("not a navpath snapshot (bad magic)")
    version = struct.unpack_from("<I", buf, 4)[0]
    layout = LAYOUTS.get(version)
    if layout is None:
        known = ", ".join(f"v{v}" for v in sorted(LAYOUTS))
        raise ValueError(f"unsupported snapshot version v{version} (this script reads {known}; "
                         "add its layout to LAYOUTS in convert_path.py)")
    c_fmt = f"<{len(layout.counts)}I"
    o_fmt = f"<{len(layout.sections)}Q"
    o0 = 8 + struct.calcsize(c_fmt)
    if len(buf) < o0 + struct.calcsize(o_fmt):
        raise ValueError("snapshot header too small")
    counts = dict(zip(layout.counts, struct.unpack_from(c_fmt, buf, 8)))
    offsets = dict(zip(layout.sections, struct.unpack_from(o_fmt, buf, o0)))
    return SnapshotHeader(version, counts, offsets, layout, len(buf))


def read_snapshot_coords(snapshot_path: str, ids: Iterable[int]) -> List[Coord]:
    with open(snapshot_path, "rb") as f, mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as mm:
        header = read_snapshot_header(mm)
        return header.layout.coords(mm, header, ids)


# --- Response parsing ------------------------------------------------------------------

def load_json(path: str):
    with open(path, "r") as f:
        return json.load(f)


def _point(v) -> Union[Coord, None]:
    """[x, y, plane] -> tuple; {min, max} box -> max corner (fallback min)."""
    if isinstance(v, (list, tuple)) and len(v) >= 3:
        return int(v[0]), int(v[1]), int(v[2])
    if isinstance(v, dict):
        # Newer macro actions provide bounding boxes; prefer max, fallback to min
        for key in ("max", "min"):
            c = v.get(key)
            if isinstance(c, (list, tuple)) and len(c) >= 3:
                return int(c[0]), int(c[1]), int(c[2])
    return None


def _extract_from_actions(obj: dict) -> List[Coord]:
    actions = obj.get("actions", [])
    if not isinstance(actions, list):
        return []
    out: List[Coord] = []
    for action in actions:
        if not isinstance(action, dict):
            continue
        # Prefer the 'to' coordinate; some actions only have a 'from' (rare)
        p = _point(action.get("to")) or _point(action.get("from"))
        if p is not None:
            out.append(p)
    return out


def _extract_from_geometry(obj: dict) -> List[Coord]:
    geom = obj.get("geometry")
    if not isinstance(geom, list):
        return []
    return [(int(v[0]), int(v[1]), int(v[2]))
            for v in geom if isinstance(v, (list, tuple)) and len(v) >= 3]


def _extract_from_path_ids(obj: dict, snapshot_path: str) -> List[Coord]:
    path_ids = obj.get("path")
    if not (isinstance(path_ids, list) and path_ids and all(isinstance(v, int) for v in path_ids)):
        return []
    if not os.path.exists(snapshot_path):
        raise FileNotFoundError(f"snapshot file not found: {snapshot_path}. "
                                "Set SNAPSHOT_PATH / --snapshot or place graph.snapshot in CWD.")
    return read_snapshot_coords(snapshot_path, path_ids)


def _extract_from_legacy_steps(data: list) -> List[Coord]:
    """Legacy list of steps with 'to.max'."""
    out: List[Coord] = []
    for step in data:
        to = step.get("to") if isinstance(step, dict) else None
        if isinstance(to, dict):
            c = to.get("max")
            if isinstance(c, (list, tuple)) and len(c) >= 3:
                out.append((int(c[0]), int(c[1]), int(c[2])))
    return out


def extract_coordinates(data: Union[dict, list], source: str = "auto",
                        snapshot_path: Union[str, None] = None) -> List[Coord]:
    snapshot_path = snapshot_path or os.environ.get("SNAPSHOT_PATH", "graph.snapshot")
    if isinstance(data, list):
        return _extract_from_legacy_steps(data) if source in ("auto", "actions") else []
    if not isinstance(data, dict):
        return []
    extractors = {
        "geometry": lambda: _extract_from_geometry(data),
        "actions": lambda: _extract_from_actions(data),
        "path": lambda: _extract_from_path_ids(data, snapshot_path),
    }
    order = ("geometry", "actions", "path") if source == "auto" else (source,)
    for name in order:
        coords = extractors[name]()
        if coords:
            return coords
    return []


def to_java_array(coordinates: List[Coord]) -> str:
    body = ",\n".join(f"    new Coordinate({int(x)}, {int(y)}, {int(p)})" for x, y, p in coordinates)
    return "Coordinate[] path = {\n" + (body + "\n" if body else "") + "};"


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("input", nargs="?", default="result.json", help="route response JSON (default: result.json)")
    ap.add_argument("-o", "--output", default="results_parsed.json",
                    help="output file (default: results_parsed.json; '-' for stdout)")
    ap.add_argument("--source", choices=("auto", "geometry", "actions", "path"), default="auto",
                    help="where to take coordinates from (default: first non-empty of geometry, actions, path)")
    ap.add_argument("--snapshot", help="snapshot for node-id paths (default: $SNAPSHOT_PATH or graph.snapshot)")
    args = ap.parse_args()

    coordinates = extract_coordinates(load_json(args.input), args.source, args.snapshot)
    java_code = to_java_array(coordinates)
    if args.output == "-":
        print(java_code)
    else:
        with open(args.output, "w") as f:
            f.write(java_code)


if __name__ == "__main__":
    main()
