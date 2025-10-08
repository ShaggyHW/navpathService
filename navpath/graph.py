"""Graph neighbor generation for navpath pathfinding."""

from __future__ import annotations

import logging
from dataclasses import dataclass, field
from typing import Dict, Iterable, List, Optional, Protocol, Sequence, Set, Tuple

from .cost import CostModel
from .db import Database, LodestoneNodeRow
from .nodes import NodeChainResolver, Bounds2D
from .options import SearchOptions
from .path import NodeRef, StepType, Tile

LOGGER = logging.getLogger(__name__)


@dataclass(slots=True)
class Edge:
    """Represents a traversable edge in the navigation graph."""

    type: StepType
    from_tile: Tile
    to_tile: Tile
    cost_ms: int
    node: Optional[NodeRef] = None
    metadata: dict[str, object] = field(default_factory=dict)


class GraphProvider(Protocol):
    """Protocol describing objects capable of yielding neighbor edges."""

    def neighbors(self, tile: Tile, goal: Tile, options: SearchOptions) -> Iterable[Edge]:
        """Yield edges reachable from ``tile`` respecting ``options``."""


@dataclass(frozen=True)
class _Movement:
    name: str
    bit: int
    delta: Tuple[int, int, int]


_MOVEMENTS: Sequence[_Movement] = (
    _Movement("north", 1 << 0, (0, 1, 0)),
    _Movement("south", 1 << 1, (0, -1, 0)),
    _Movement("east", 1 << 2, (1, 0, 0)),
    _Movement("west", 1 << 3, (-1, 0, 0)),
    _Movement("northeast", 1 << 4, (1, 1, 0)),
    _Movement("northwest", 1 << 5, (-1, 1, 0)),
    _Movement("southeast", 1 << 6, (1, -1, 0)),
    _Movement("southwest", 1 << 7, (-1, -1, 0)),
)

# Deterministic output ordering (cardinals first, then diagonals).
_MOVEMENT_ORDER: Sequence[str] = (
    "north",
    "south",
    "east",
    "west",
    "northeast",
    "northwest",
    "southeast",
    "southwest",
)

_MOVEMENT_BY_NAME = {movement.name: movement for movement in _MOVEMENTS}


class TileNotFoundError(LookupError):
    """Raised when neighbor generation is attempted for a missing tile."""

    def __init__(self, tile: Tile) -> None:
        message = f"Tile not found in database: {tile}"
        super().__init__(message)
        self.tile = tile


class SqliteGraphProvider:
    """Graph provider backed by the SQLite world database."""

    def __init__(self, database: Database, cost_model: CostModel) -> None:
        self._db = database
        self._cost_model = cost_model
        self._lodestone_nodes_cache: Optional[List[LodestoneNodeRow]] = None
        self._lodestone_by_dest: Dict[Tile, List[LodestoneNodeRow]] = {}
        self._lodestone_id_to_dest: Dict[int, Tile] = {}
        self._lodestone_dest_valid: Dict[int, bool] = {}
        # Small tables we can cache fully for performance
        self._ifslot_nodes_cache = None
        self._item_nodes_cache = None

    def neighbors(self, tile: Tile, goal: Tile, options: SearchOptions) -> Iterable[Edge]:
        """Return edges reachable from ``tile`` according to the options."""

        tile_row = self._db.fetch_tile(*tile)
        if tile_row is None:
            LOGGER.warning("Requested neighbors for unknown tile %s", tile)
            raise TileNotFoundError(tile)

        edges: List[Edge] = []
        edges.extend(self._movement_edges(tile, tile_row))
        if options.use_doors:
            edges.extend(self._door_edges(tile))
        if options.use_lodestones:
            edges.extend(self._lodestone_edges(tile))
        # Action edges via portal semantics and chain resolution
        if options.use_objects:
            edges.extend(self._object_edges(tile, options))
        if options.use_ifslots:
            edges.extend(self._ifslot_edges(tile, options))
        if options.use_npcs:
            edges.extend(self._npc_edges(tile, options))
        if options.use_items:
            edges.extend(self._item_edges(tile, options))
        return edges

    # ------------------------------------------------------------------
    def _movement_edges(self, tile: Tile, tile_row) -> List[Edge]:
        mask = _decode_allowed_mask(tile_row.allowed_directions)
        if mask == 0:
            return []

        edges: List[Edge] = []
        for movement_name in _MOVEMENT_ORDER:
            movement = _MOVEMENT_BY_NAME[movement_name]
            if mask & movement.bit == 0:
                continue
            dest = (
                tile[0] + movement.delta[0],
                tile[1] + movement.delta[1],
                tile[2] + movement.delta[2],
            )
            if self._db.fetch_tile(*dest) is None:
                LOGGER.debug(
                    "Skipping movement %s from %s due to missing destination tile %s",
                    movement.name,
                    tile,
                    dest,
                )
                continue
            cost = self._cost_model.movement_cost(tile, dest)
            edges.append(
                Edge(
                    type="move",
                    from_tile=tile,
                    to_tile=dest,
                    cost_ms=cost,
                )
            )

        return edges

    def _door_edges(self, tile: Tile) -> List[Edge]:
        edges: List[Edge] = []
        seen: Set[Tuple[Tile, int]] = set()
        for row in self._db.iter_door_nodes_touching(tile):
            if tile == row.tile_inside:
                dest = row.tile_outside
            elif tile == row.tile_outside:
                dest = row.tile_inside
            else:
                continue

            key = (dest, row.id)
            if key in seen:
                continue
            seen.add(key)

            if self._db.fetch_tile(*dest) is None:
                LOGGER.debug(
                    "Skipping door node %s from %s due to missing destination tile %s",
                    row.id,
                    tile,
                    dest,
                )
                continue

            cost = self._cost_model.door_cost(row.cost)
            edges.append(
                Edge(
                    type="door",
                    from_tile=tile,
                    to_tile=dest,
                    cost_ms=cost,
                    node=NodeRef("door", row.id),
                    metadata={
                        "door_direction": row.direction,
                        "real_id_open": row.real_id_open,
                        "real_id_closed": row.real_id_closed,
                    },
                )
            )

        edges.sort(key=lambda edge: (edge.to_tile, edge.node.id if edge.node else -1))
        return edges

    def _lodestone_edges(self, tile: Tile) -> List[Edge]:
        self._ensure_lodestone_cache()

        source_nodes = self._lodestone_by_dest.get(tile)
        if not source_nodes:
            return []

        edges: List[Edge] = []
        seen: Set[Tuple[int, int]] = set()

        for source in sorted(source_nodes, key=lambda node: node.id):
            cost = self._cost_model.lodestone_cost(source.cost)
            for target in self._lodestone_nodes_cache or []:
                if target.id == source.id:
                    continue
                pair = (source.id, target.id)
                if pair in seen:
                    continue
                dest = self._lodestone_id_to_dest.get(target.id, target.dest)
                if not self._lodestone_dest_valid.get(target.id, False):
                    LOGGER.debug(
                        "Skipping lodestone edge %s -> %s due to missing destination tile",
                        source.id,
                        dest,
                    )
                    continue
                edges.append(
                    Edge(
                        type="lodestone",
                        from_tile=tile,
                        to_tile=dest,
                        cost_ms=cost,
                        node=NodeRef("lodestone", source.id),
                        metadata={
                            "source_lodestone": source.lodestone,
                            "target_lodestone": target.lodestone,
                        },
                    )
                )
                seen.add(pair)

        edges.sort(key=lambda edge: (edge.node.id if edge.node else -1, edge.to_tile))
        return edges

    def _ensure_lodestone_cache(self) -> None:
        if self._lodestone_nodes_cache is not None:
            return

        nodes = list(self._db.iter_lodestone_nodes())
        nodes.sort(key=lambda node: node.id)
        self._lodestone_nodes_cache = nodes
        dest_map: Dict[Tile, List[LodestoneNodeRow]] = {}
        id_map: Dict[int, Tile] = {}
        valid_map: Dict[int, bool] = {}
        for node in nodes:
            dest_map.setdefault(node.dest, []).append(node)
            id_map[node.id] = node.dest
            valid_map[node.id] = self._db.fetch_tile(*node.dest) is not None
        self._lodestone_by_dest = dest_map
        self._lodestone_id_to_dest = id_map
        self._lodestone_dest_valid = valid_map


    # ------------------------------------------------------------------
    # Action edges (object/ifslot/npc/item) using NodeChainResolver
    def _object_edges(self, tile: Tile, options: SearchOptions) -> List[Edge]:
        edges: List[Edge] = []
        resolver = NodeChainResolver(self._db, self._cost_model, options)
        seen: Set[Tuple[int, Tile]] = set()
        for row in self._db.iter_object_nodes_touching(tile):
            ref = NodeRef("object", row.id)
            resolution = resolver.resolve(ref)
            if not resolution.is_success or resolution.destination is None:
                continue
            dest = self._select_dest_tile(resolution.destination, fallback_plane=tile[2])
            if dest is None:
                continue
            key = (row.id, dest)
            if key in seen:
                continue
            seen.add(key)
            edges.append(
                Edge(
                    type="object",
                    from_tile=tile,
                    to_tile=dest,
                    cost_ms=resolution.total_cost_ms,
                    node=ref,
                )
            )
        edges.sort(key=lambda e: (e.node.id if e.node else -1, e.to_tile))
        return edges

    def _ifslot_edges(self, tile: Tile, options: SearchOptions) -> List[Edge]:
        edges: List[Edge] = []
        resolver = NodeChainResolver(self._db, self._cost_model, options)
        self._ensure_ifslot_cache()
        for row in self._ifslot_nodes_cache or []:
            ref = NodeRef("ifslot", row.id)
            resolution = resolver.resolve(ref)
            if not resolution.is_success or resolution.destination is None:
                continue
            dest = self._select_dest_tile(resolution.destination, fallback_plane=tile[2])
            if dest is None:
                continue
            edges.append(
                Edge(
                    type="ifslot",
                    from_tile=tile,
                    to_tile=dest,
                    cost_ms=resolution.total_cost_ms,
                    node=ref,
                )
            )
        edges.sort(key=lambda e: (e.node.id if e.node else -1, e.to_tile))
        return edges

    def _npc_edges(self, tile: Tile, options: SearchOptions) -> List[Edge]:
        edges: List[Edge] = []
        resolver = NodeChainResolver(self._db, self._cost_model, options)
        seen: Set[Tuple[int, Tile]] = set()
        for row in self._db.iter_npc_nodes_touching(tile):
            ref = NodeRef("npc", row.id)
            resolution = resolver.resolve(ref)
            if not resolution.is_success or resolution.destination is None:
                continue
            dest = self._select_dest_tile(resolution.destination, fallback_plane=tile[2])
            if dest is None:
                continue
            key = (row.id, dest)
            if key in seen:
                continue
            seen.add(key)
            edges.append(
                Edge(
                    type="npc",
                    from_tile=tile,
                    to_tile=dest,
                    cost_ms=resolution.total_cost_ms,
                    node=ref,
                )
            )
        edges.sort(key=lambda e: (e.node.id if e.node else -1, e.to_tile))
        return edges

    def _item_edges(self, tile: Tile, options: SearchOptions) -> List[Edge]:
        edges: List[Edge] = []
        resolver = NodeChainResolver(self._db, self._cost_model, options)
        self._ensure_item_cache()
        for row in self._item_nodes_cache or []:
            ref = NodeRef("item", row.id)
            resolution = resolver.resolve(ref)
            if not resolution.is_success or resolution.destination is None:
                continue
            dest = self._select_dest_tile(resolution.destination, fallback_plane=tile[2])
            if dest is None:
                continue
            edges.append(
                Edge(
                    type="item",
                    from_tile=tile,
                    to_tile=dest,
                    cost_ms=resolution.total_cost_ms,
                    node=ref,
                )
            )
        edges.sort(key=lambda e: (e.node.id if e.node else -1, e.to_tile))
        return edges

    def _select_dest_tile(self, bounds: Bounds2D, fallback_plane: int) -> Optional[Tile]:
        """Pick a deterministic existing tile within ``bounds``.

        Scans from min to max along x then y; uses ``fallback_plane`` if
        bounds.plane is None. Returns None if no tile exists in bounds.
        """

        if not bounds.is_valid():
            return None
        plane = bounds.plane if bounds.plane is not None else fallback_plane
        for x in range(bounds.min_x, bounds.max_x + 1):
            for y in range(bounds.min_y, bounds.max_y + 1):
                candidate = (x, y, plane)
                if self._db.fetch_tile(*candidate) is not None:
                    return candidate
        return None

    def _ensure_ifslot_cache(self) -> None:
        if self._ifslot_nodes_cache is not None:
            return
        self._ifslot_nodes_cache = list(self._db.iter_ifslot_nodes())
        self._ifslot_nodes_cache.sort(key=lambda n: n.id)

    def _ensure_item_cache(self) -> None:
        if self._item_nodes_cache is not None:
            return
        self._item_nodes_cache = list(self._db.iter_item_nodes())
        self._item_nodes_cache.sort(key=lambda n: n.id)


def _decode_allowed_mask(value: Optional[object]) -> int:
    """Interpret the ``allowed_directions`` field as a bitmask."""

    if value is None:
        return 0
    if isinstance(value, int):
        return value
    if isinstance(value, bytes):
        try:
            value = value.decode("utf-8")
        except UnicodeDecodeError:
            LOGGER.warning("Unable to decode allowed_directions bytes: %s", value)
            return 0
    if isinstance(value, str):
        stripped = value.strip()
        if not stripped:
            return 0
        try:
            return int(stripped, 10)
        except ValueError:
            mask = 0
            parts = [part.strip().lower() for part in stripped.split(",")]
            for part in parts:
                movement = _MOVEMENT_BY_NAME.get(part)
                if movement is None:
                    LOGGER.debug("Unknown movement token in allowed_directions: %s", part)
                    continue
                mask |= movement.bit
            return mask
    LOGGER.warning("Unsupported allowed_directions value type: %s", type(value))
    return 0
