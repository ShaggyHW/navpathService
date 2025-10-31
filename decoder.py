#!/usr/bin/env python3 
# python decoder.py input.txt -o output.csv
import argparse
import base64
import binascii
import json
import re
import sqlite3
import sys
from typing import List, Tuple

def _maybe_hex_text_to_bytes(b: bytes) -> bytes:
    """
    Accepts:
      - raw binary (returns as-is)
      - SQLite X'..' literal
      - \\x.. escaped hex
      - bare hex string
      - base64 (fallback)
    """
    # If already binary-like (non-printable bytes), return as-is
    if any(ch > 0x7E or ch < 0x20 for ch in b if ch not in (9, 10, 13)):
        return b

    s = b.decode("utf-8", errors="replace").strip()

    # SQLite X'...'
    m = re.fullmatch(r"[xX]'([0-9A-Fa-f\s]+)'", s)
    if m:
        hexpart = re.sub(r"\s+", "", m.group(1))
        try:
            return binascii.unhexlify(hexpart)
        except binascii.Error:
            pass

    # \x.. escaped hex
    if "\\x" in s:
        hexpart = re.sub(r"\\x", "", s)
        hexpart = re.sub(r"\s+", "", hexpart)
        if re.fullmatch(r"[0-9A-Fa-f]+", hexpart or "0"):
            try:
                return binascii.unhexlify(hexpart)
            except binascii.Error:
                pass

    # Bare hex
    bare = re.sub(r"\s+", "", s)
    if len(bare) >= 2 and re.fullmatch(r"[0-9A-Fa-f]+", bare) and len(bare) % 2 == 0:
        try:
            return binascii.unhexlify(bare)
        except binascii.Error:
            pass

    # Base64 fallback
    try:
        return base64.b64decode(s, validate=True)
    except Exception:
        pass

    # Default: return raw bytes
    return b


def decode_triplets_le_i32(buf: bytes) -> List[Tuple[int, int, int]]:
    if len(buf) % 12 != 0:
        raise ValueError(f"Blob length {len(buf)} is not a multiple of 12 bytes.")
    pts = []
    for i in range(0, len(buf), 12):
        x = int.from_bytes(buf[i:i+4], "little", signed=True)
        y = int.from_bytes(buf[i+4:i+8], "little", signed=True)
        plane = int.from_bytes(buf[i+8:i+12], "little", signed=True)
        pts.append((x, y, plane))
    return pts


def main():
    ap = argparse.ArgumentParser(
        description="Decode intra path blob (little-endian i32 triplets x,y,plane) from SQL dump."
    )
    ap.add_argument("database", help="Path to SQLite database.")
    ap.add_argument("-o", "--output", help="Output file path (default: stdout).")
    ap.add_argument("-f", "--format", choices=["csv", "json", "coordinates"], default="csv",
                    help="Output format (default: csv).")
    ap.add_argument("-t", "--table", default="cluster_intraconnections",
                    help="Table containing path blobs (default: cluster_intraconnections).")
    ap.add_argument("--entrance-from", type=int,
                    help="Filter rows by entrance_from value.")
    args = ap.parse_args()

    if not re.fullmatch(r"[A-Za-z0-9_]+", args.table):
        raise ValueError("Table name must contain only letters, numbers, or underscores.")

    with sqlite3.connect(args.database) as conn:
        conn.row_factory = sqlite3.Row
        query = f"SELECT entrance_from, entrance_to, path_blob FROM {args.table}"
        params = []
        if args.entrance_from is not None:
            query += " WHERE entrance_from = ?"
            params.append(args.entrance_from)
        query += " ORDER BY entrance_from, entrance_to"
        rows = conn.execute(query, params).fetchall()

    if not rows:
        raise ValueError("No rows found matching the given criteria.")

    decoded_rows = []
    for row in rows:
        raw_blob = row["path_blob"]
        if raw_blob is None:
            points = []
        else:
            if isinstance(raw_blob, memoryview):
                raw_blob = raw_blob.tobytes()
            if isinstance(raw_blob, str):
                raw_blob = raw_blob.encode("utf-8")
            if isinstance(raw_blob, bytearray):
                raw_blob = bytes(raw_blob)
            points = decode_triplets_le_i32(_maybe_hex_text_to_bytes(raw_blob))
        decoded_rows.append((row["entrance_from"], row["entrance_to"], points))

    if args.format == "json":
        out_text = json.dumps([
            {
                "entrance_from": entrance_from,
                "entrance_to": entrance_to,
                "points": [
                    {"x": x, "y": y, "plane": plane}
                    for x, y, plane in points
                ],
            }
            for entrance_from, entrance_to, points in decoded_rows
        ], indent=2)
    elif args.format == "csv":
        lines = ["entrance_from,entrance_to,x,y,plane"]
        for entrance_from, entrance_to, points in decoded_rows:
            if points:
                for x, y, plane in points:
                    lines.append(f"{entrance_from},{entrance_to},{x},{y},{plane}")
            else:
                lines.append(f"{entrance_from},{entrance_to},,,")
        out_text = "\n".join(lines)
    else:
        blocks = []
        for entrance_from, entrance_to, points in decoded_rows:
            header = (
                f"Coordinate[] path_from_{entrance_from}_to_{entrance_to} = {{"
            )
            if points:
                body_lines = [f"    new Coordinate({x}, {y}, {plane})" for x, y, plane in points]
                block = "\n".join([header, ",\n".join(body_lines), "};"])
            else:
                block = "\n".join([header, "};"])
            blocks.append(block)
        out_text = "\n\n".join(blocks)

    if args.output:
        with open(args.output, "w", encoding="utf-8") as f:
            f.write(out_text)
    else:
        print(out_text)


if __name__ == "__main__":
    try:
        main()
    except Exception as e:
        print(f"Error: {e}", file=sys.stderr)
        sys.exit(1)
