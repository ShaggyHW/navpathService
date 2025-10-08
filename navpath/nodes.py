"""Node metadata helpers and chain resolution utilities."""

from __future__ import annotations

import logging
from dataclasses import dataclass
from typing import List, Optional, Set, Tuple

from .cost import CostModel
from .db import (
    Database,
    DoorNodeRow,
    IfslotNodeRow,
    ItemNodeRow,
    LodestoneNodeRow,
    NodeRow,
    NpcNodeRow,
    ObjectNodeRow,
)
from .options import SearchOptions
from .path import NodeRef, Tile

LOGGER = logging.getLogger(__name__)


@dataclass(slots=True)
class Bounds2D:
    """Inclusive rectangular bounds on the tile grid."""

    min_x: int
    max_x: int
    min_y: int
    max_y: int
    plane: Optional[int]

    def is_valid(self) -> bool:
        """Return ``True`` when the bounds describe a non-empty area."""

        return self.min_x <= self.max_x and self.min_y <= self.max_y

    def contains(self, tile: Tile) -> bool:
        """Return whether ``tile`` resides within the bounds."""

        x, y, plane = tile
        if plane is not None and self.plane is not None and plane != self.plane:
            return False
        return self.min_x <= x <= self.max_x and self.min_y <= y <= self.max_y

    @classmethod
    def single_tile(cls, tile: Tile) -> "Bounds2D":
        """Create bounds covering a single tile coordinate."""

        x, y, plane = tile
        return cls(x, x, y, y, plane)

    @classmethod
    def from_optional(
        cls,
        min_x: Optional[int],
        max_x: Optional[int],
        min_y: Optional[int],
        max_y: Optional[int],
        plane: Optional[int],
    ) -> Optional["Bounds2D"]:
        """Return bounds when all coordinate components are present."""

        if (
            min_x is None
            or max_x is None
            or min_y is None
            or max_y is None
        ):
            return None
        bounds = cls(min_x, max_x, min_y, max_y, plane)
        if bounds.is_valid():
            return bounds
        LOGGER.warning(
            "Invalid bounds ignored (min>max): %s",
            {
                "min_x": min_x,
                "max_x": max_x,
                "min_y": min_y,
                "max_y": max_y,
                "plane": plane,
            },
        )
        return None

    def merge(self, other: "Bounds2D") -> "Bounds2D":
        """Return bounds covering both ``self`` and ``other``."""

        plane: Optional[int]
        if self.plane == other.plane:
            plane = self.plane
        else:
            plane = None
        return Bounds2D(
            min(self.min_x, other.min_x),
            max(self.max_x, other.max_x),
            min(self.min_y, other.min_y),
            max(self.max_y, other.max_y),
            plane,
        )


@dataclass(slots=True)
class ChainLink:
    """Represents a single node encountered during chain resolution."""

    ref: NodeRef
    cost_ms: int
    destination: Optional[Bounds2D]
    row: NodeRow


@dataclass(slots=True)
class ChainResolution:
    """Result of attempting to resolve a node chain."""

    start: NodeRef
    links: List[ChainLink]
    total_cost_ms: int
    destination: Optional[Bounds2D]
    failure_reason: Optional[str] = None

    @property
    def is_success(self) -> bool:
        """Whether the chain resolved with a usable destination."""

        return self.failure_reason is None and self.destination is not None

    @property
    def terminal_ref(self) -> Optional[NodeRef]:
        """Return the reference for the final node in the chain."""

        return self.links[-1].ref if self.links else None


class NodeChainResolver:
    """Resolve `next_node` chains to produce composite action metadata."""

    def __init__(
        self,
        database: Database,
        cost_model: CostModel,
        options: Optional[SearchOptions] = None,
    ) -> None:
        self._db = database
        self._cost_model = cost_model
        self._options = options or cost_model.options
        self._max_depth = self._options.max_chain_depth

    def resolve(self, start: NodeRef) -> ChainResolution:
        """Resolve a chain beginning at ``start``.

        The returned resolution aggregates the total cost of all nodes in
        the chain (respecting overrides captured within the associated
        :class:`CostModel`) and surfaces the terminal destination bounds
        if one exists. Chains that violate safety rules (missing nodes,
        cycles, depth overflows, or absent destinations) return a
        ``ChainResolution`` with ``failure_reason`` populated and
        ``is_success`` evaluating to ``False``.
        """

        current_ref = NodeRef(self._normalise_type(start.type), start.id)
        chain_start = NodeRef(self._normalise_type(start.type), start.id)
        visited: Set[Tuple[str, int]] = set()
        links: List[ChainLink] = []
        total_cost = 0
        depth = 0
        failure_reason: Optional[str] = None

        while True:
            if depth >= self._max_depth:
                failure_reason = "chain-depth-exceeded"
                LOGGER.warning(
                    "Node chain depth exceeded (limit=%s) for start=%s",
                    self._max_depth,
                    start,
                )
                break

            key = (current_ref.type, current_ref.id)
            if key in visited:
                failure_reason = "cycle-detected"
                LOGGER.warning("Detected node chain cycle starting at %s", start)
                break
            visited.add(key)

            row = self._db.fetch_node(current_ref.type, current_ref.id)
            if row is None:
                failure_reason = "missing-node"
                LOGGER.warning(
                    "Node chain aborted due to missing node: type=%s id=%s start=%s",
                    current_ref.type,
                    current_ref.id,
                    start,
                )
                break

            cost = self._node_cost(current_ref.type, row)
            destination = self._destination_bounds(current_ref.type, row)
            chain_link = ChainLink(ref=current_ref, cost_ms=cost, destination=destination, row=row)
            links.append(chain_link)
            total_cost += cost

            next_type = getattr(row, "next_node_type", None)
            next_id = getattr(row, "next_node_id", None)
            if next_type is None or next_id is None:
                break

            current_ref = NodeRef(self._normalise_type(next_type), next_id)
            depth += 1

        destination = links[-1].destination if links else None
        if failure_reason is None and destination is None:
            failure_reason = "missing-destination"
            LOGGER.warning(
                "Node chain resolved without destination bounds: start=%s terminal=%s",
                start,
                links[-1].ref if links else None,
            )

        return ChainResolution(
            start=chain_start,
            links=links,
            total_cost_ms=total_cost,
            destination=destination,
            failure_reason=failure_reason,
        )

    def _normalise_type(self, node_type: str) -> str:
        return node_type.strip().lower()

    def _node_cost(self, node_type: str, row: NodeRow) -> int:
        if node_type == "door":
            assert isinstance(row, DoorNodeRow)
            return self._cost_model.door_cost(row.cost)
        if node_type == "lodestone":
            assert isinstance(row, LodestoneNodeRow)
            return self._cost_model.lodestone_cost(row.cost)
        if node_type == "object":
            assert isinstance(row, ObjectNodeRow)
            return self._cost_model.object_cost(row.cost)
        if node_type == "ifslot":
            assert isinstance(row, IfslotNodeRow)
            return self._cost_model.ifslot_cost(row.cost)
        if node_type == "npc":
            assert isinstance(row, NpcNodeRow)
            return self._cost_model.npc_cost(row.cost)
        if node_type == "item":
            assert isinstance(row, ItemNodeRow)
            return self._cost_model.item_cost(row.cost)
        raise ValueError(f"Unsupported node_type: {node_type}")

    def _destination_bounds(self, node_type: str, row: NodeRow) -> Optional[Bounds2D]:
        if node_type == "lodestone":
            assert isinstance(row, LodestoneNodeRow)
            return Bounds2D.single_tile(row.dest)
        if node_type == "object":
            assert isinstance(row, ObjectNodeRow)
            return Bounds2D.from_optional(
                row.dest_min_x,
                row.dest_max_x,
                row.dest_min_y,
                row.dest_max_y,
                row.dest_plane,
            )
        if node_type == "ifslot":
            assert isinstance(row, IfslotNodeRow)
            return Bounds2D.from_optional(
                row.dest_min_x,
                row.dest_max_x,
                row.dest_min_y,
                row.dest_max_y,
                row.dest_plane,
            )
        if node_type == "npc":
            assert isinstance(row, NpcNodeRow)
            return Bounds2D.from_optional(
                row.dest_min_x,
                row.dest_max_x,
                row.dest_min_y,
                row.dest_max_y,
                row.dest_plane,
            )
        if node_type == "item":
            assert isinstance(row, ItemNodeRow)
            return Bounds2D.from_optional(
                row.dest_min_x,
                row.dest_max_x,
                row.dest_min_y,
                row.dest_max_y,
                row.dest_plane,
            )
        if node_type == "door":
            assert isinstance(row, DoorNodeRow)
            inside_bounds = Bounds2D.single_tile(row.tile_inside)
            outside_bounds = Bounds2D.single_tile(row.tile_outside)
            merged = inside_bounds.merge(outside_bounds)
            return merged
        raise ValueError(f"Unsupported node_type: {node_type}")
