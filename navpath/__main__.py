"""Command-line interface for navpath pathfinding.

Usage examples:
  python -m navpath --start "3200,3200,0" --goal "3210,3211,0" --json
  python -m navpath --start "3200,3200,0" --goal "3210,3211,0" --db tiles.db
"""
from __future__ import annotations

import argparse
import json
import logging
import sys
from typing import Tuple, Dict, Any
from pathlib import Path

from .api import find_path
from .options import SearchOptions
from .path import Tile

LOGGER = logging.getLogger(__name__)


def _parse_tile(value: str) -> Tile:
    try:
        parts = [int(p.strip()) for p in value.split(",")]
        if len(parts) != 3:
            raise ValueError
        return (parts[0], parts[1], parts[2])
    except Exception as exc:  # noqa: BLE001
        raise argparse.ArgumentTypeError(
            f"Expected tile in form 'x,y,plane', got: {value!r}"
        ) from exc


def _build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="navpath",
        description="A* pathfinding over SQLite world graph",
    )

    # Required endpoints
    p.add_argument("--start", type=_parse_tile, required=True, help="Start tile: x,y,plane")
    p.add_argument("--goal", type=_parse_tile, required=True, help="Goal tile: x,y,plane")

    # IO
    p.add_argument("--db", type=str, default=None, help="Path to SQLite DB (defaults to worldReachableTiles.db)")
    p.add_argument("--json", action="store_true", help="Output result as JSON")
    p.add_argument("--json-actions-only", action="store_true", help="Output only the actions array as JSON")
    p.add_argument("--out", "--output", dest="out_path", type=str, default=None, help="Write output to file instead of stdout")

    # Limits
    p.add_argument("--max-expansions", type=int, default=None, help="Maximum node expansions")
    p.add_argument("--timeout-ms", type=int, default=None, help="Timeout in milliseconds")
    p.add_argument("--max-chain-depth", type=int, default=None, help="Max next_node chain depth")

    # Node-type toggles (disable flags; default is enabled)
    p.add_argument("--no-doors", dest="use_doors", action="store_false", help="Disable door edges")
    p.add_argument("--no-lodestones", dest="use_lodestones", action="store_false", help="Disable lodestone edges")
    p.add_argument("--no-objects", dest="use_objects", action="store_false", help="Disable object edges")
    p.add_argument("--no-ifslots", dest="use_ifslots", action="store_false", help="Disable ifslot edges")
    p.add_argument("--no-npcs", dest="use_npcs", action="store_false", help="Disable NPC edges")
    p.add_argument("--no-items", dest="use_items", action="store_false", help="Disable item edges")

    # Cost overrides (ms)
    p.add_argument("--door-cost", type=int, default=None, help="Override door cost (ms)")
    p.add_argument("--lodestone-cost", type=int, default=None, help="Override lodestone cost (ms)")
    p.add_argument("--object-cost", type=int, default=None, help="Override object action cost (ms)")
    p.add_argument("--ifslot-cost", type=int, default=None, help="Override ifslot action cost (ms)")
    p.add_argument("--npc-cost", type=int, default=None, help="Override NPC action cost (ms)")
    p.add_argument("--item-cost", type=int, default=None, help="Override item action cost (ms)")

    # Logging
    p.add_argument("--log-level", default="INFO", choices=["CRITICAL", "ERROR", "WARNING", "INFO", "DEBUG"], help="Logging level")

    # Requirements context (R3/R4)
    p.add_argument(
        "--requirements-file",
        dest="requirements_file",
        type=str,
        default=None,
        help=(
            "Path to JSON file with an array of {key:string, value:int|bool}. "
            "Booleans are coerced to 1/0. When combined with --requirements-json, last-wins per key."
        ),
    )
    p.add_argument(
        "--requirements-json",
        dest="requirements_json",
        type=str,
        default=None,
        help=(
            "JSON string of an array of {key:string, value:int|bool}. "
            "Booleans coerced to 1/0. When combined with --requirements-file, last-wins per key."
        ),
    )

    # Defaults for toggles (enabled unless disabled)
    p.set_defaults(use_doors=True, use_lodestones=True, use_objects=True, use_ifslots=True, use_npcs=True, use_items=True)

    return p


def _options_from_args(args: argparse.Namespace) -> SearchOptions:
    opts = SearchOptions()

    # Limits
    if args.max_expansions is not None:
        opts.max_expansions = args.max_expansions
    if args.timeout_ms is not None:
        opts.timeout_ms = args.timeout_ms
    if args.max_chain_depth is not None:
        opts.max_chain_depth = args.max_chain_depth

    # Toggles
    opts.use_doors = args.use_doors
    opts.use_lodestones = args.use_lodestones
    opts.use_objects = args.use_objects
    opts.use_ifslots = args.use_ifslots
    opts.use_npcs = args.use_npcs
    opts.use_items = args.use_items

    # Cost overrides
    opts.door_cost_override = args.door_cost
    opts.lodestone_cost_override = args.lodestone_cost
    opts.object_cost_override = args.object_cost
    opts.ifslot_cost_override = args.ifslot_cost
    opts.npc_cost_override = args.npc_cost
    opts.item_cost_override = args.item_cost

    # Requirements ingestion (R3/R4)
    req_map: Dict[str, int] = {}

    if getattr(args, "requirements_file", None):
        try:
            text = Path(args.requirements_file).read_text(encoding="utf-8")
        except Exception as exc:  # noqa: BLE001
            raise ValueError(f"Failed to read --requirements-file: {args.requirements_file!r}: {exc}") from exc
        try:
            payload: Any = json.loads(text)
        except Exception as exc:  # noqa: BLE001
            raise ValueError(f"Invalid JSON in --requirements-file {args.requirements_file!r}: {exc}") from exc
        _merge_requirements_payload(req_map, payload, source=f"--requirements-file {args.requirements_file}")

    if getattr(args, "requirements_json", None):
        try:
            payload: Any = json.loads(args.requirements_json)
        except Exception as exc:  # noqa: BLE001
            raise ValueError(f"Invalid JSON in --requirements-json: {exc}") from exc
        _merge_requirements_payload(req_map, payload, source="--requirements-json")

    if req_map:
        # Convert to an array of {key,value} per spec
        opts.extras["requirements"] = [{"key": k, "value": v} for k, v in req_map.items()]

    return opts


def _merge_requirements_payload(target: Dict[str, int], payload: Any, *, source: str) -> None:
    """Validate and merge a requirements JSON payload into target map.

    payload must be a list of {"key": string, "value": int|bool}.
    Booleans are coerced to 1/0. Last-wins per key (later merges override).
    Raises ValueError on invalid structure or types.
    """

    if not isinstance(payload, list):
        raise ValueError(f"{source} must be a JSON array of objects; got {type(payload).__name__}")
    for idx, item in enumerate(payload):
        if not isinstance(item, dict):
            raise ValueError(f"{source}[{idx}] must be an object with 'key' and 'value'")
        key = item.get("key")
        if not isinstance(key, str) or not key:
            raise ValueError(f"{source}[{idx}].key must be a non-empty string")
        if "value" not in item:
            raise ValueError(f"{source}[{idx}].value is required")
        val = item["value"]
        if isinstance(val, bool):
            coerced = 1 if val else 0
        elif isinstance(val, int):
            coerced = val
        else:
            raise ValueError(
                f"{source}[{idx}].value must be int or bool (coerced to 1/0); got {type(val).__name__}"
            )
        target[key] = int(coerced)


def _format_human(result) -> str:
    # Stable, concise human-readable format
    path_len = len(result.path) if result.path is not None else 0
    lines = [
        f"reason: {result.reason}",
        f"expanded: {result.expanded}",
        f"path_len: {path_len}",
        f"total_cost_ms: {result.cost_ms}",
    ]
    if result.path is not None:
        lines.append("path:")
        for t in result.path:
            lines.append(f"  - [{t[0]}, {t[1]}, {t[2]}]")
    return "\n".join(lines) + "\n"


def _print_human(result) -> None:
    print(_format_human(result), end="")


def main(argv: list[str] | None = None) -> int:
    parser = _build_parser()
    args = parser.parse_args(argv)

    logging.basicConfig(level=getattr(logging, args.log_level))

    try:
        options = _options_from_args(args)
    except ValueError as exc:
        # Parse/validation error for requirements inputs
        print(f"Error: {exc}", file=sys.stderr)
        return 2

    result = find_path(args.start, args.goal, options=options, db_path=args.db)

    if getattr(args, "json_actions_only", False):
        out_payload = [a.to_json_dict() for a in result.actions]
        out_text = json.dumps(out_payload, separators=(",", ":"), indent=2) + "\n"
    elif args.json:
        out_text = json.dumps(result.to_json_dict(), separators=(",", ":"), indent=2) + "\n"
    else:
        out_text = _format_human(result)

    if getattr(args, "out_path", None):
        out_file = Path(args.out_path)
        out_file.parent.mkdir(parents=True, exist_ok=True)
        out_file.write_text(out_text, encoding="utf-8")
    else:
        print(out_text, end="")

    # Exit code 0 if path found or properly reported; non-zero only on parse errors
    return 0


if __name__ == "__main__":  # pragma: no cover
    sys.exit(main())
