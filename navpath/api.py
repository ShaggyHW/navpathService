"""Public API for navpath pathfinding.

Exposes `find_path(start, goal, options=None, db_path=None)` which opens the
SQLite database, validates inputs, runs A* search, logs summary metrics at
INFO, and returns a `PathResult`.
"""
from __future__ import annotations

import logging
import time
from pathlib import Path
from typing import Optional, Union

from .astar import astar
from .cost import CostModel
from .graph import SqliteGraphProvider
from .options import SearchOptions
from .path import PathResult, Tile
from .db import Database

LOGGER = logging.getLogger(__name__)


def _default_db_path() -> Path:
    # Project root is one level above the package directory
    return Path(__file__).resolve().parents[1] / "worldReachableTiles.db"


def _validate_tile(tile: Tile) -> bool:
    return (
        isinstance(tile, tuple)
        and len(tile) == 3
        and all(isinstance(v, int) for v in tile)
    )


def find_path(
    start: Tile,
    goal: Tile,
    options: Optional[SearchOptions] = None,
    db_path: Optional[Union[str, Path]] = None,
) -> PathResult:
    """Compute a path from `start` to `goal`.

    - Validates inputs and DB tiles
    - Assembles `Database`, `CostModel`, and `SqliteGraphProvider`
    - Executes deterministic A* and logs summary metrics at INFO
    - Returns a `PathResult`
    """

    # Input validation (Requirement 5.4)
    if not (_validate_tile(start) and _validate_tile(goal)):
        LOGGER.warning("Invalid input tiles: start=%r goal=%r", start, goal)
        return PathResult(path=None, actions=[], reason="invalid-input", expanded=0, cost_ms=0)

    # Resolve DB path: explicit arg > options.extras > default
    effective_db_path: Union[str, Path] = (
        db_path
        if db_path is not None
        else (options.extras.get("db_path") if (options and isinstance(options.extras, dict)) else None)
    ) or _default_db_path()

    opts = options or SearchOptions()
    # Provide start tile via extras so graph can limit global lodestone edges
    try:
        if not isinstance(opts.extras, dict):
            opts.extras = {}
        opts.extras["start_tile"] = start
        # Normalize requirements list to a dict map for fast lookups (R3)
        req_list = opts.extras.get("requirements")
        if isinstance(req_list, list):
            req_map: dict[str, int] = {}
            for item in req_list:
                try:
                    # Expect objects with {key, value}
                    key = item.get("key") if isinstance(item, dict) else None
                    val = item.get("value") if isinstance(item, dict) else None
                except Exception:
                    key = None
                    val = None
                if isinstance(key, str) and key:
                    # Only accept ints; upstream CLI coerces booleans to ints
                    if isinstance(val, int):
                        req_map[key] = int(val)
            if req_map:
                opts.extras["requirements_map"] = req_map
    except Exception:
        # Fallback safety: ensure extras exists
        opts.extras = {"start_tile": start}
    cost_model = CostModel(options=opts)

    t0_ns = time.perf_counter_ns()

    db = Database.connect(effective_db_path)
    try:
        # Validate start/goal exist in tiles (Requirement 2.4)
        if db.fetch_tile(*start) is None or db.fetch_tile(*goal) is None:
            duration_ms = int((time.perf_counter_ns() - t0_ns) / 1_000_000)
            LOGGER.info(
                "find_path metrics: start=%s goal=%s reason=%s expanded=%d path_len=%d total_cost_ms=%d duration_ms=%d req_filtered=%d db=%s",
                start,
                goal,
                "tile-not-found",
                0,
                0,
                0,
                duration_ms,
                0,
                str(effective_db_path),
            )
            return PathResult(path=None, actions=[], reason="tile-not-found", expanded=0, cost_ms=0)

        graph = SqliteGraphProvider(db, cost_model)
        result = astar(start, goal, graph, cost_model, opts)

        duration_ms = int((time.perf_counter_ns() - t0_ns) / 1_000_000)
        path_len = len(result.path) if result.path is not None else 0
        req_filtered = getattr(graph, "req_filtered_count", 0)

        # Summary metrics (Requirement 5.3)
        LOGGER.info(
            "find_path metrics: start=%s goal=%s reason=%s expanded=%d path_len=%d total_cost_ms=%d duration_ms=%d req_filtered=%d db=%s",
            start,
            goal,
            result.reason,
            result.expanded,
            path_len,
            result.cost_ms,
            duration_ms,
            req_filtered,
            str(effective_db_path),
        )
        return result
    finally:
        db.close()


__all__ = ["find_path"]
