"""A* search implementation for the navpath service.

Implements deterministic A* using a binary heap with stable tie-breaking.
Reconstructs the path and action steps (including movement and action edges)
from recorded parent links. Respects `max_expansions` and `timeout_ms`.
"""

from __future__ import annotations

import heapq
import itertools
import time
from dataclasses import dataclass
from math import inf
from typing import Dict, Iterable, List, Optional, Tuple

from .cost import CostModel
from .graph import Edge, GraphProvider
from .options import SearchOptions
from .path import ActionStep, PathResult, Tile, NodeRef


@dataclass(slots=True)
class _QueueItem:
    f: int
    h: int
    g: int
    seq: int
    tile: Tile

    def key(self) -> Tuple[int, int, int, int, Tile]:
        # Deterministic ordering: (f, h, g, seq, tile)
        # Note: tile ordering acts as a final deterministic tiebreaker.
        return (self.f, self.h, self.g, self.seq, self.tile)


def astar(
    start: Tile,
    goal: Tile,
    graph: GraphProvider,
    cost_model: CostModel,
    options: Optional[SearchOptions] = None,
) -> PathResult:
    """Run A* from `start` to `goal` using the provided `graph`.

    Returns a `PathResult` with path, actions, reason (if not found),
    expanded count, and total cost in milliseconds.
    """

    opts = options or cost_model.options

    # Early exit if start == goal
    if start == goal:
        return PathResult(path=[start], actions=[], reason=None, expanded=0, cost_ms=0)

    start_time = time.monotonic_ns()
    timeout_ns = int(opts.timeout_ms) * 1_000_000

    # Best-known cost to each tile.
    g_score: Dict[Tile, int] = {start: 0}

    # Parent mapping for reconstruction: child -> (parent, edge)
    parent: Dict[Tile, Tuple[Tile, Edge]] = {}

    # Closed set of fully-explored tiles
    closed: Dict[Tile, bool] = {}

    # Priority queue
    counter = itertools.count()
    start_h = cost_model.heuristic(start, goal)
    start_item = _QueueItem(f=start_h, h=start_h, g=0, seq=next(counter), tile=start)
    open_heap: List[Tuple[int, int, int, int, Tile]] = [start_item.key()]

    expanded = 0

    while open_heap:
        # Timeout check
        if timeout_ns and (time.monotonic_ns() - start_time) >= timeout_ns:
            return PathResult(path=None, actions=[], reason="timeout", expanded=expanded, cost_ms=0)

        f, h, g, _, current = heapq.heappop(open_heap)
        # If we've already found a better path to current, skip
        if g != g_score.get(current, inf):
            continue

        # Goal check when node is dequeued with optimal g
        if current == goal:
            path, actions, total_cost = _reconstruct(current, parent)
            return PathResult(path=path, actions=actions, reason=None, expanded=expanded, cost_ms=total_cost)

        # Expansion limit check
        expanded += 1
        if expanded > opts.max_expansions:
            return PathResult(path=None, actions=[], reason="max-expansions", expanded=expanded, cost_ms=0)

        closed[current] = True

        # Generate neighbors deterministically (graph ensures deterministic ordering)
        for edge in graph.neighbors(current, goal, opts):
            neighbor = edge.to_tile
            tentative_g = g + edge.cost_ms

            # Only proceed when we have found a strictly better path.
            # We intentionally allow re-opening nodes even if they were
            # previously closed, because heuristics that include teleports
            # can be inconsistent. The pop-time check against g_score will
            # discard stale entries safely.
            if tentative_g >= g_score.get(neighbor, inf):
                continue

            g_score[neighbor] = tentative_g
            parent[neighbor] = (current, edge)

            nh = cost_model.heuristic(neighbor, goal)
            nf = tentative_g + nh
            item = _QueueItem(f=nf, h=nh, g=tentative_g, seq=next(counter), tile=neighbor)
            heapq.heappush(open_heap, item.key())

    # Unreachable
    return PathResult(path=None, actions=[], reason="unreachable", expanded=expanded, cost_ms=0)


def _reconstruct(end: Tile, parent: Dict[Tile, Tuple[Tile, Edge]]) -> Tuple[List[Tile], List[ActionStep], int]:
    # Reconstruct reverse path and edges
    rev_tiles: List[Tile] = [end]
    rev_edges: List[Edge] = []

    total_cost = 0
    cur = end
    while cur in parent:
        prev, edge = parent[cur]
        rev_tiles.append(prev)
        rev_edges.append(edge)
        total_cost += edge.cost_ms
        cur = prev

    rev_tiles.reverse()
    # Build action steps in forward order; edges were collected reverse (child->parent)
    rev_edges.reverse()

    actions: List[ActionStep] = []
    cur_from = rev_tiles[0]
    for edge in rev_edges:
        chain = []
        try:
            if isinstance(edge.metadata, dict):
                chain = edge.metadata.get("chain") or []
        except Exception:
            chain = []

        if chain:
            # Emit each chain link as its own step. Intermediate links do not move tiles.
            last_index = len(chain) - 1
            for idx, link in enumerate(chain):
                step_type = link.get("type", edge.type)
                node_id = link.get("id")
                link_meta = link.get("metadata", {}) if isinstance(link, dict) else {}
                cost = int(link.get("cost_ms", 0)) if isinstance(link, dict) else 0

                to_tile = edge.to_tile if idx == last_index else cur_from
                step = ActionStep(
                    type=step_type,
                    from_tile=cur_from,
                    to_tile=to_tile,
                    cost_ms=cost,
                    node=NodeRef(step_type, int(node_id)) if node_id is not None else edge.node,
                    metadata=link_meta,
                )
                actions.append(step)
            # After the last link, we've moved to edge.to_tile
            cur_from = edge.to_tile
        else:
            # No chain: single step as before
            step = ActionStep(
                type=edge.type,
                from_tile=cur_from,
                to_tile=edge.to_tile,
                cost_ms=edge.cost_ms,
                node=edge.node,
                metadata=edge.metadata,
            )
            actions.append(step)
            cur_from = edge.to_tile

    return rev_tiles, actions, total_cost
