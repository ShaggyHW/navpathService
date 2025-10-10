"""Path-related data models for the navpath service.

These dataclasses define the publicly shared shapes for pathfinding
results. They are deliberately JSON-friendly so that callers can
serialize `PathResult` instances via :func:`dataclasses.asdict` or the
provided helper methods without losing fidelity.
"""

from __future__ import annotations

from dataclasses import asdict, dataclass, field
from typing import Any, Dict, List, Literal, Optional, Tuple

Tile = Tuple[int, int, int]
"""Alias for a tile coordinate expressed as ``(x, y, plane)``."""

StepType = Literal[
    "move",
    "door",
    "lodestone",
    "object",
    "ifslot",
    "npc",
    "item",
]
"""Literal string type capturing the supported action step kinds."""


@dataclass(slots=True)
class NodeRef:
    """Reference to a node entry within the SQLite backing store."""

    type: str
    """Table name or logical node type, e.g. ``"door"`` or ``"object"``."""

    id: int
    """Primary key of the referenced node within its table."""

    def to_json_dict(self) -> Dict[str, Any]:
        """Return a JSON-serializable mapping representing the reference."""

        return {"type": self.type, "id": self.id}


@dataclass(slots=True)
class ActionStep:
    """Represents a single transition taken while traversing a path."""

    type: StepType
    """Kind of transition that occurred."""

    from_tile: Tile
    """Origin tile coordinate prior to executing the step."""

    to_tile: Tile
    """Destination tile coordinate after executing the step."""

    cost_ms: int
    """Cost of the step in milliseconds."""

    node: Optional[NodeRef] = None
    """Associated node metadata for non-movement steps, when available."""

    metadata: Dict[str, Any] = field(default_factory=dict)
    """Arbitrary metadata for the step (e.g., lodestone name)."""

    def to_json_dict(self) -> Dict[str, Any]:
        """Return a JSON-serializable dictionary for this action step."""

        def _rect_from_tile(tile: Tile) -> Dict[str, List[int]]:
            x, y, p = tile
            return {"min": [x, y, p], "max": [x, y, p]}

        def _rect_from_bounds(prefix: str, fallback_plane: int, row: Dict[str, Any]) -> Optional[Dict[str, List[int]]]:
            try:
                min_x = row.get(f"{prefix}_min_x")
                max_x = row.get(f"{prefix}_max_x")
                min_y = row.get(f"{prefix}_min_y")
                max_y = row.get(f"{prefix}_max_y")
                plane = row.get(f"{prefix}_plane")
            except Exception:
                return None
            if (
                isinstance(min_x, int)
                and isinstance(max_x, int)
                and isinstance(min_y, int)
                and isinstance(max_y, int)
            ):
                p = plane if isinstance(plane, int) else fallback_plane
                return {"min": [int(min_x), int(min_y), int(p)], "max": [int(max_x), int(max_y), int(p)]}
            return None

        db_row: Optional[Dict[str, Any]] = None
        try:
            if isinstance(self.metadata, dict):
                maybe = self.metadata.get("db_row")
                if isinstance(maybe, dict):
                    db_row = maybe
        except Exception:
            db_row = None

        from_rect = _rect_from_tile(self.from_tile)
        to_rect = _rect_from_tile(self.to_tile)

        if db_row is not None:
            # Prefer explicit origin/destination bounds when provided by node rows
            maybe_from = _rect_from_bounds("orig", self.from_tile[2], db_row)
            if maybe_from is not None:
                from_rect = maybe_from
            # Only expand 'to' to destination bounds when the step actually moves
            if self.from_tile != self.to_tile:
                maybe_to = _rect_from_bounds("dest", self.to_tile[2], db_row)
                if maybe_to is not None:
                    to_rect = maybe_to

        payload = {
            "type": self.type,
            "from": from_rect,
            "to": to_rect,
            "cost_ms": self.cost_ms,
        }
        if self.node is not None:
            payload["node"] = self.node.to_json_dict()
        if self.metadata:
            payload["metadata"] = self.metadata
        return payload


@dataclass(slots=True)
class PathResult:
    """Outcome of a pathfinding request.

    All fields are designed to be directly JSON-serializable when using
    :func:`dataclasses.asdict` or the :meth:`to_json_dict` helper.
    """

    path: Optional[List[Tile]]
    """List of traversed tile coordinates, or ``None`` if unreachable."""

    actions: List[ActionStep]
    """Ordered collection of steps describing how to execute the path."""

    reason: Optional[str]
    """Reason string explaining why a path was not produced, when relevant."""

    expanded: int
    """Total node expansions performed by the search."""

    cost_ms: int
    """Aggregate cost of the resulting path in milliseconds."""

    def to_json_dict(self) -> Dict[str, Any]:
        """Return a JSON-serializable dictionary representation."""

        base: Dict[str, Any] = asdict(self)
        if self.path is not None:
            base["path"] = [list(tile) for tile in self.path]
        base["actions"] = [action.to_json_dict() for action in self.actions]
        return base
