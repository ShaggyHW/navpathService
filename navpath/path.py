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

        payload = {
            "type": self.type,
            "from": list(self.from_tile),
            "to": list(self.to_tile),
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
