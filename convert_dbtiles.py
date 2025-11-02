#!/usr/bin/env python3

"""Convert dbtiles.txt entries into the Coordinate[] format."""

from __future__ import annotations

import argparse
from pathlib import Path


def _parse_line(raw: str, line_number: int) -> tuple[int, int, int] | None:
    stripped = raw.strip()
    if not stripped:
        return None

    parts = stripped.split()
    if len(parts) < 3:
        raise ValueError(f"Line {line_number} has fewer than 3 columns: {raw!r}")

    try:
        x, y, plane = (int(parts[0]), int(parts[1]), int(parts[2]))
    except ValueError as exc:
        raise ValueError(f"Line {line_number} contains non-integer values: {raw!r}") from exc

    return x, y, plane


def convert(input_path: Path, output_path: Path) -> None:
    coords: list[tuple[int, int, int]] = []

    with input_path.open("r", encoding="utf-8") as src:
        for idx, line in enumerate(src, start=1):
            parsed = _parse_line(line, idx)
            if parsed is None:
                continue
            coords.append(parsed)

    with output_path.open("w", encoding="utf-8") as dest:
        dest.write("Coordinate[] path = {\n")
        for i, (x, y, plane) in enumerate(coords):
            trailing = "," if i < len(coords) - 1 else ""
            dest.write(f"    new Coordinate({x}, {y}, {plane}){trailing}\n")
        dest.write("};\n")


def main() -> None:
    repo_root = Path(__file__).resolve().parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--input",
        "-i",
        type=Path,
        default=repo_root / "dbtiles.txt",
        help="Path to the dbtiles.txt source file",
    )
    parser.add_argument(
        "--output",
        "-o",
        type=Path,
        default=repo_root / "results_parsed.json",
        help="Path to write the Coordinate[] representation",
    )

    args = parser.parse_args()
    convert(args.input, args.output)


if __name__ == "__main__":
    main()
