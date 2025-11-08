import json
import mmap
import os
import struct
from typing import Iterable, List, Tuple, Union


def load_json(path: str):
    with open(path, 'r') as f:
        return json.load(f)

def _extract_from_actions(obj: dict) -> List[Tuple[int,int,int]]:
    out: List[Tuple[int,int,int]] = []
    actions = obj.get('actions', [])
    if not isinstance(actions, list):
        return out

    for action in actions:
        if not isinstance(action, dict):
            continue

        # Prefer explicit 'to' coordinate first
        to = action.get('to')
        if isinstance(to, (list, tuple)) and len(to) >= 3:
            out.append((int(to[0]), int(to[1]), int(to[2])))
            continue
        if isinstance(to, dict):
            # Newer macro actions provide bounding boxes; prefer max, fallback to min
            max_coords = to.get('max')
            min_coords = to.get('min')
            if isinstance(max_coords, (list, tuple)) and len(max_coords) >= 3:
                out.append((int(max_coords[0]), int(max_coords[1]), int(max_coords[2])))
                continue
            if isinstance(min_coords, (list, tuple)) and len(min_coords) >= 3:
                out.append((int(min_coords[0]), int(min_coords[1]), int(min_coords[2])))
                continue

        # Some actions may only have a 'from' location (rare); include as a last resort
        frm = action.get('from')
        if isinstance(frm, (list, tuple)) and len(frm) >= 3:
            out.append((int(frm[0]), int(frm[1]), int(frm[2])))
            continue
        if isinstance(frm, dict):
            max_coords = frm.get('max')
            min_coords = frm.get('min')
            if isinstance(max_coords, (list, tuple)) and len(max_coords) >= 3:
                out.append((int(max_coords[0]), int(max_coords[1]), int(max_coords[2])))
                continue
            if isinstance(min_coords, (list, tuple)) and len(min_coords) >= 3:
                out.append((int(min_coords[0]), int(min_coords[1]), int(min_coords[2])))
                continue

    return out


def _extract_from_geometry(obj: dict) -> List[Tuple[int,int,int]]:
    geom = obj.get('geometry')
    if not isinstance(geom, list):
        return []
    out: List[Tuple[int,int,int]] = []
    for v in geom:
        if isinstance(v, (list, tuple)) and len(v) >= 3:
            out.append((int(v[0]), int(v[1]), int(v[2])))
    return out


def _read_snapshot_coords_for_ids(snapshot_path: str, ids: Iterable[int]) -> List[Tuple[int,int,int]]:
    # Snapshot header layout (see rust/navpath-core/src/snapshot/manifest.rs)
    # magic[4] + version[u32] + counts[5*u32] + offsets[19*u64]
    HEADER_SIZE = 4 + 4 + 5*4 + 19*8
    with open(snapshot_path, 'rb') as f:
        mm = mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ)
        try:
            if mm.size() < HEADER_SIZE:
                raise ValueError('snapshot header too small')
            magic = mm[0:4]
            if magic != b'NPSS':
                raise ValueError('bad snapshot magic')
            version = struct.unpack_from('<I', mm, 4)[0]
            if version != 5:
                raise ValueError(f'unsupported snapshot version: {version}')

            c0 = 8
            nodes = struct.unpack_from('<I', mm, c0 + 0)[0]
            # walk_edges = struct.unpack_from('<I', mm, c0 + 4)[0]
            # macro_edges = struct.unpack_from('<I', mm, c0 + 8)[0]
            # req_tags   = struct.unpack_from('<I', mm, c0 + 12)[0]
            # landmarks  = struct.unpack_from('<I', mm, c0 + 16)[0]
            o0 = c0 + 20
            off_nodes_ids   = struct.unpack_from('<Q', mm, o0 + 0)[0]
            off_nodes_x     = struct.unpack_from('<Q', mm, o0 + 8)[0]
            off_nodes_y     = struct.unpack_from('<Q', mm, o0 + 16)[0]
            off_nodes_plane = struct.unpack_from('<Q', mm, o0 + 24)[0]

            # Bounds checking helpers
            def _fits(off: int, bytes_len: int) -> bool:
                return off <= mm.size() and (mm.size() - off) >= bytes_len

            nodes_bytes = nodes * 4
            if not _fits(off_nodes_x, nodes_bytes):
                raise ValueError('nodes_x out of bounds')
            if not _fits(off_nodes_y, nodes_bytes):
                raise ValueError('nodes_y out of bounds')
            if not _fits(off_nodes_plane, nodes_bytes):
                raise ValueError('nodes_plane out of bounds')

            out: List[Tuple[int,int,int]] = []
            for nid in ids:
                i = int(nid)
                if i < 0 or i >= nodes:
                    # Skip invalid ids gracefully
                    continue
                x = struct.unpack_from('<i', mm, off_nodes_x + i * 4)[0]
                y = struct.unpack_from('<i', mm, off_nodes_y + i * 4)[0]
                p = struct.unpack_from('<i', mm, off_nodes_plane + i * 4)[0]
                out.append((x, y, p))
            return out
        finally:
            mm.close()


def extract_coordinates(data: Union[dict, list]) -> List[Tuple[int,int,int]]:
    # Case 1: new service response object with geometry
    if isinstance(data, dict):
        coords = _extract_from_geometry(data)
        if coords:
            return coords

    # Case 2: back-compat with old actions format
        coords = _extract_from_actions(data)
        if coords:
            return coords

        # Case 3: newest format with numeric node ids under 'path'
        path_ids = data.get('path') if isinstance(data, dict) else None
        if isinstance(path_ids, list) and all(isinstance(v, int) for v in path_ids):
            snapshot_path = os.environ.get('SNAPSHOT_PATH', 'graph.snapshot')
            if not os.path.exists(snapshot_path):
                raise FileNotFoundError(f"snapshot file not found: {snapshot_path}. Set SNAPSHOT_PATH or place graph.snapshot in CWD.")
            return _read_snapshot_coords_for_ids(snapshot_path, path_ids)

    # Case 4: legacy list of steps with 'to.max'
    if isinstance(data, list):
        out: List[Tuple[int,int,int]] = []
        for step in data:
            if not isinstance(step, dict):
                continue
            to = step.get('to')
            if isinstance(to, dict):
                max_coords = to.get('max')
                if isinstance(max_coords, (list, tuple)) and len(max_coords) >= 3:
                    out.append((int(max_coords[0]), int(max_coords[1]), int(max_coords[2])))
        if out:
            return out

    return []


def to_java_array(coordinates: List[Tuple[int,int,int]]):
    java_code = "Coordinate[] path = {\n"
    for i, (x, y, plane) in enumerate(coordinates):
        java_code += f"    new Coordinate({int(x)}, {int(y)}, {int(plane)})"
        if i < len(coordinates) - 1:
            java_code += ","
        java_code += "\n"
    java_code += "};"
    return java_code


def main():
    data = load_json('result.json')
    coordinates = extract_coordinates(data)
    java_code = to_java_array(coordinates)

    # Write to results_parsed.json
    with open('results_parsed.json', 'w') as f:
        f.write(java_code)


if __name__ == '__main__':
    main()
