#!/usr/bin/env python3
import json
from pathlib import Path

INPUT = Path("result.json")
OUTPUT = Path("results_parsed.json")

def to_tuple(coord_dict):
    # Prefer "min" if present (your data has min==max)
    if isinstance(coord_dict, dict) and "min" in coord_dict:
        return tuple(coord_dict["min"])
    # Fallback if already a list/tuple
    return tuple(coord_dict)

def main():
    data = json.loads(INPUT.read_text(encoding="utf-8"))
    actions = data.get("actions", [])

    points = []
    if actions:
        first_from = actions[0].get("from")
        if first_from:
            points.append(to_tuple(first_from))

        for a in actions:
            to_pt = a.get("to")
            if to_pt:
                points.append(to_tuple(to_pt))

    # Dedupe consecutive duplicates
    deduped = []
    for pt in points:
        if not deduped or deduped[-1] != pt:
            deduped.append(pt)

    # Build Java code
    java_lines = ["Coordinate[] path = {"]
    for i, (x, y, z) in enumerate(deduped):
        comma = "," if i < len(deduped) - 1 else ""
        java_lines.append(f"    new Coordinate({x}, {y}, {z}){comma}")
    java_lines.append("};")
    java_code = "\n".join(java_lines)

    OUTPUT.write_text(java_code + "\n", encoding="utf-8")
    print(f"Wrote {OUTPUT} with {len(deduped)} points.")

if __name__ == "__main__":
    main()
