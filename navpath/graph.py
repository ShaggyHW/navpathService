"""Graph neighbor generation for navpath pathfinding."""

from __future__ import annotations

import logging
from collections import OrderedDict
from dataclasses import dataclass, field, asdict
from typing import Dict, Iterable, List, Optional, Protocol, Sequence, Set, Tuple

from .cost import CostModel
from .db import Database, LodestoneNodeRow, ObjectNodeRow, NpcNodeRow
from .nodes import NodeChainResolver, Bounds2D
from .options import SearchOptions
from .path import NodeRef, StepType, Tile
from .requirements import evaluate_requirement

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


# Precompute fast lookup from 8-bit tiledata (RuneApps collision bits)
# to this module's movement bitmask. The external tiledata mapping is:
#   bit0=west, bit1=north, bit2=east, bit3=south,
#   bit4=northwest, bit5=northeast, bit6=southeast, bit7=southwest
# Our internal bit positions are bound to _MOVEMENTS definitions.
def _build_tiledata_lookup() -> tuple[int, ...]:
    table = [0] * 256
    b_west = _MOVEMENT_BY_NAME["west"].bit
    b_north = _MOVEMENT_BY_NAME["north"].bit
    b_east = _MOVEMENT_BY_NAME["east"].bit
    b_south = _MOVEMENT_BY_NAME["south"].bit
    b_northwest = _MOVEMENT_BY_NAME["northwest"].bit
    b_northeast = _MOVEMENT_BY_NAME["northeast"].bit
    b_southeast = _MOVEMENT_BY_NAME["southeast"].bit
    b_southwest = _MOVEMENT_BY_NAME["southwest"].bit
    for v in range(256):
        m = 0
        if v & (1 << 0):
            m |= b_west
        if v & (1 << 1):
            m |= b_north
        if v & (1 << 2):
            m |= b_east
        if v & (1 << 3):
            m |= b_south
        if v & (1 << 4):
            m |= b_northwest
        if v & (1 << 5):
            m |= b_northeast
        if v & (1 << 6):
            m |= b_southeast
        if v & (1 << 7):
            m |= b_southwest
        table[v] = m
    return tuple(table)


_TILEDATA_TO_MASK: tuple[int, ...] = _build_tiledata_lookup()


def _mask_from_tiledata(value: Optional[object]) -> Optional[int]:
    """Return internal movement mask mapped from 8-bit tiledata, or None.

    Accepts ints or numeric-like values; returns None if value is None or not
    coercible to int. Values are masked to 0..255.
    """
    if value is None:
        return None
    try:
        iv = int(value) & 0xFF
    except Exception:
        return None
    return _TILEDATA_TO_MASK[iv]


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
        # Track nodes that are referenced by another node's next_node; such nodes
        # are NOT chain-heads and must not be invoked directly as starting nodes.
        self._non_head_nodes: Dict[str, Set[int]] = {}
        self._chain_head_index_built: bool = False
        # Requirements gating state
        self.req_filtered_count: int = 0
        self._requirement_cache: Dict[int, object] = {}
        # Per-plane tile existence cache (x,y) set with small LRU eviction
        self._plane_tile_sets: Dict[int, Set[Tuple[int, int]]] = {}
        self._plane_lru: "OrderedDict[int, None]" = OrderedDict()
        self._plane_cache_capacity: int = 8
        # Per-tile touching node caches to avoid repeated SQLite hits during expansion
        self._touch_cache_capacity: int = 4096
        self._obj_touch_cache: Dict[Tile, List[ObjectNodeRow]] = {}
        self._obj_touch_lru: "OrderedDict[Tile, None]" = OrderedDict()
        self._npc_touch_cache: Dict[Tile, List[NpcNodeRow]] = {}
        self._npc_touch_lru: "OrderedDict[Tile, None]" = OrderedDict()
        # Reusable NodeChainResolver and per-provider chain resolution memo
        self._resolver: Optional[NodeChainResolver] = None
        self._chain_resolution_cache: Dict[Tuple[str, int], object] = {}

    def neighbors(self, tile: Tile, goal: Tile, options: SearchOptions) -> Iterable[Edge]:
        """Return edges reachable from ``tile`` according to the options."""

        # Ensure we have computed chain-head restrictions before generating edges
        self._ensure_chain_head_index()

        # Build requirement context map from options extras
        ctx_map = self._build_ctx_map(options)

        tile_row = self._db.fetch_tile(*tile)
        if tile_row is None:
            LOGGER.warning("Requested neighbors for unknown tile %s", tile)
            raise TileNotFoundError(tile)

        edges: List[Edge] = []
        edges.extend(self._movement_edges(tile, tile_row))
        if options.use_doors:
            edges.extend(self._door_edges(tile, ctx_map))
        if options.use_lodestones:
            edges.extend(self._lodestone_edges(tile, options, ctx_map))
        # Action edges via portal semantics and chain resolution
        if options.use_objects:
            edges.extend(self._object_edges(tile, options, ctx_map))
        if options.use_ifslots:
            edges.extend(self._ifslot_edges(tile, options, ctx_map))
        if options.use_npcs:
            edges.extend(self._npc_edges(tile, options, ctx_map))
        if options.use_items:
            edges.extend(self._item_edges(tile, options, ctx_map))
        return edges

    # ------------------------------------------------------------------
    # Resolver/memoization helpers
    def _get_resolver(self, options: SearchOptions) -> NodeChainResolver:
        # Create once per provider; options are stable for a search lifecycle.
        if self._resolver is None:
            self._resolver = NodeChainResolver(self._db, self._cost_model, options)
        return self._resolver

    def _resolve_chain_cached(self, ref: NodeRef, resolver: NodeChainResolver):
        key = (ref.type.strip().lower(), int(ref.id))
        cached = self._chain_resolution_cache.get(key)
        if cached is not None:
            return cached
        resolution = resolver.resolve(ref)
        self._chain_resolution_cache[key] = resolution
        return resolution

    # ------------------------------------------------------------------
    def _movement_edges(self, tile: Tile, tile_row) -> List[Edge]:
        # Prefer integer tiledata mapping for performance; fallback to legacy
        # allowed_directions parsing when tiledata is absent.
        td_mask = _mask_from_tiledata(getattr(tile_row, "tiledata", None))
        mask = td_mask if td_mask is not None else _decode_allowed_mask(tile_row.allowed_directions)
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
            if not self._tile_exists(dest[0], dest[1], dest[2]):
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

    def _door_edges(self, tile: Tile, ctx_map: Dict[str, int]) -> List[Edge]:
        edges: List[Edge] = []
        seen: Set[Tuple[Tile, int]] = set()
        for row in self._db.iter_door_nodes_touching(tile):
            # Requirement gating (head-only)
            if not self._passes_requirement(getattr(row, "requirement_id", None), ctx_map):
                continue
            # Skip doors that are referenced by next_node (non-head)
            if self._is_non_head("door", row.id):
                continue
            computed_dir: Optional[str] = None
            if tile == row.tile_inside:
                dest = row.tile_outside
                computed_dir = "OUT"
            elif tile == row.tile_outside:
                dest = row.tile_inside
                computed_dir = "IN"
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
            door_meta: dict[str, object] = {
                "door_direction": (computed_dir or row.direction),
                "db_door_direction": row.direction,
                "real_id_open": row.real_id_open,
                "real_id_closed": row.real_id_closed,
            }
            if getattr(row, "open_action", None) is not None:
                door_meta["action"] = row.open_action
            # Include full DB row data
            try:
                door_meta["db_row"] = asdict(row)
            except Exception:
                pass
            edges.append(
                Edge(
                    type="door",
                    from_tile=tile,
                    to_tile=dest,
                    cost_ms=cost,
                    node=NodeRef("door", row.id),
                    metadata=door_meta,
                )
            )

        edges.sort(key=lambda edge: (edge.to_tile, edge.node.id if edge.node else -1))
        return edges

    def _lodestone_edges(self, tile: Tile, options: SearchOptions, ctx_map: Dict[str, int]) -> List[Edge]:
        self._ensure_lodestone_cache()

        # Only generate global lodestone teleports from the start tile. Since
        # lodestones have no origin constraints and costs are independent of
        # the origin, any optimal path that uses a lodestone can begin with it.
        # This drastically reduces branching without sacrificing optimality.
        start_tile = options.extras.get("start_tile") if isinstance(options.extras, dict) else None
        if start_tile is not None and tuple(start_tile) != tuple(tile):
            return []

        # Lodestones have no origin constraints; they can be used from anywhere.
        # Generate direct edges from the current tile to every valid lodestone destination.
        edges: List[Edge] = []

        for target in sorted(self._lodestone_nodes_cache or [], key=lambda node: node.id):
            # Skip lodestones that are referenced by next_node (non-head)
            if self._is_non_head("lodestone", target.id):
                continue
            # Requirement gating (head-only)
            if not self._passes_requirement(getattr(target, "requirement_id", None), ctx_map):
                continue
            if not self._lodestone_dest_valid.get(target.id, False):
                LOGGER.debug(
                    "Skipping lodestone destination %s due to missing destination tile",
                    target.id,
                )
                continue
            dest = self._lodestone_id_to_dest.get(target.id, target.dest)
            # Optional: skip no-op teleports to the current tile
            if dest == tile:
                continue
            cost = self._cost_model.lodestone_cost(target.cost)
            edges.append(
                Edge(
                    type="lodestone",
                    from_tile=tile,
                    to_tile=dest,
                    cost_ms=cost,
                    node=NodeRef("lodestone", target.id),
                    metadata={
                        "lodestone": target.lodestone,
                        "target_lodestone": target.lodestone,
                        # Include full DB row data
                        "db_row": (asdict(target) if target is not None else {}),
                    },
                )
            )

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
    def _object_edges(self, tile: Tile, options: SearchOptions, ctx_map: Dict[str, int]) -> List[Edge]:
        edges: List[Edge] = []
        resolver = self._get_resolver(options)
        seen: Set[Tuple[int, Tile]] = set()
        for row in self._get_object_nodes_touching_cached(tile):
            # Requirement gating before resolving chains
            if not self._passes_requirement(getattr(row, "requirement_id", None), ctx_map):
                continue
            if self._is_non_head("object", row.id):
                continue
            ref = NodeRef("object", row.id)
            resolution = self._resolve_chain_cached(ref, resolver)
            if not resolution.is_success or resolution.destination is None:
                continue
            dest = self._select_dest_tile(resolution.destination, fallback_plane=tile[2])
            if dest is None:
                continue
            key = (row.id, dest)
            if key in seen:
                continue
            seen.add(key)
            # Attach actionable metadata when available
            obj_meta: dict[str, object] = {}
            if row.action is not None:
                obj_meta["action"] = row.action
            if row.object_id is not None:
                obj_meta["object_id"] = row.object_id
            if row.object_name is not None:
                obj_meta["object_name"] = row.object_name
            if row.match_type is not None:
                obj_meta["match_type"] = row.match_type
            # Include full DB row data
            try:
                obj_meta["db_row"] = asdict(row)
            except Exception:
                pass
            # Embed chain sequence for output reconstruction
            obj_meta["chain"] = self._build_chain_metadata(resolution)
            edges.append(
                Edge(
                    type="object",
                    from_tile=tile,
                    to_tile=dest,
                    cost_ms=resolution.total_cost_ms,
                    node=ref,
                    metadata=obj_meta,
                )
            )
        edges.sort(key=lambda e: (e.node.id if e.node else -1, e.to_tile))
        return edges

    def _ifslot_edges(self, tile: Tile, options: SearchOptions, ctx_map: Dict[str, int]) -> List[Edge]:
        edges: List[Edge] = []
        resolver = self._get_resolver(options)
        self._ensure_ifslot_cache()
        for row in self._ifslot_nodes_cache or []:
            # Requirement gating before resolving chains
            if not self._passes_requirement(getattr(row, "requirement_id", None), ctx_map):
                continue
            if self._is_non_head("ifslot", row.id):
                continue
            ref = NodeRef("ifslot", row.id)
            resolution = self._resolve_chain_cached(ref, resolver)
            if not resolution.is_success or resolution.destination is None:
                continue
            dest = self._select_dest_tile(resolution.destination, fallback_plane=tile[2])
            if dest is None:
                continue
            # Include interface interaction details
            if_meta: dict[str, object] = {}
            if row.interface_id is not None:
                if_meta["interface_id"] = row.interface_id
            if row.component_id is not None:
                if_meta["component_id"] = row.component_id
            if row.slot_id is not None:
                if_meta["slot_id"] = row.slot_id
            if row.click_id is not None:
                if_meta["click_id"] = row.click_id
            # Include full DB row data
            try:
                if_meta["db_row"] = asdict(row)
            except Exception:
                pass
            # Embed chain sequence
            if_meta["chain"] = self._build_chain_metadata(resolution)
            edges.append(
                Edge(
                    type="ifslot",
                    from_tile=tile,
                    to_tile=dest,
                    cost_ms=resolution.total_cost_ms,
                    node=ref,
                    metadata=if_meta,
                )
            )
        edges.sort(key=lambda e: (e.node.id if e.node else -1, e.to_tile))
        return edges

    def _npc_edges(self, tile: Tile, options: SearchOptions, ctx_map: Dict[str, int]) -> List[Edge]:
        edges: List[Edge] = []
        resolver = self._get_resolver(options)
        seen: Set[Tuple[int, Tile]] = set()
        for row in self._get_npc_nodes_touching_cached(tile):
            # Requirement gating before resolving chains
            if not self._passes_requirement(getattr(row, "requirement_id", None), ctx_map):
                continue
            if self._is_non_head("npc", row.id):
                continue
            ref = NodeRef("npc", row.id)
            resolution = self._resolve_chain_cached(ref, resolver)
            if not resolution.is_success or resolution.destination is None:
                continue
            dest = self._select_dest_tile(resolution.destination, fallback_plane=tile[2])
            if dest is None:
                continue
            key = (row.id, dest)
            if key in seen:
                continue
            seen.add(key)
            # Attach NPC action metadata when present
            npc_meta: dict[str, object] = {}
            if row.action is not None:
                npc_meta["action"] = row.action
            if row.npc_id is not None:
                npc_meta["npc_id"] = row.npc_id
            if row.npc_name is not None:
                npc_meta["npc_name"] = row.npc_name
            if row.match_type is not None:
                npc_meta["match_type"] = row.match_type
            # Include full DB row data
            try:
                npc_meta["db_row"] = asdict(row)
            except Exception:
                pass
            # Embed chain sequence
            npc_meta["chain"] = self._build_chain_metadata(resolution)
            edges.append(
                Edge(
                    type="npc",
                    from_tile=tile,
                    to_tile=dest,
                    cost_ms=resolution.total_cost_ms,
                    node=ref,
                    metadata=npc_meta,
                )
            )
        edges.sort(key=lambda e: (e.node.id if e.node else -1, e.to_tile))
        return edges

    def _item_edges(self, tile: Tile, options: SearchOptions, ctx_map: Dict[str, int]) -> List[Edge]:
        edges: List[Edge] = []
        resolver = self._get_resolver(options)
        self._ensure_item_cache()
        for row in self._item_nodes_cache or []:
            # Requirement gating before resolving chains
            if not self._passes_requirement(getattr(row, "requirement_id", None), ctx_map):
                continue
            if self._is_non_head("item", row.id):
                continue
            ref = NodeRef("item", row.id)
            resolution = self._resolve_chain_cached(ref, resolver)
            if not resolution.is_success or resolution.destination is None:
                continue
            dest = self._select_dest_tile(resolution.destination, fallback_plane=tile[2])
            if dest is None:
                continue
            # Attach item action metadata when present
            item_meta: dict[str, object] = {}
            if row.action is not None:
                item_meta["action"] = row.action
            if row.item_id is not None:
                item_meta["item_id"] = row.item_id
            # Include full DB row data
            try:
                item_meta["db_row"] = asdict(row)
            except Exception:
                pass
            # Embed chain sequence
            item_meta["chain"] = self._build_chain_metadata(resolution)
            edges.append(
                Edge(
                    type="item",
                    from_tile=tile,
                    to_tile=dest,
                    cost_ms=resolution.total_cost_ms,
                    node=ref,
                    metadata=item_meta,
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
                if self._tile_exists(candidate[0], candidate[1], candidate[2]):
                    return candidate
        return None

    # ------------------------------------------------------------------
    # Tile existence cache
    def _tile_exists(self, x: int, y: int, plane: int) -> bool:
        """Fast-path existence check using a per-plane (x,y) set.

        Lazily builds the set for the plane on first use via
        ``Database.iter_tiles_by_plane`` and keeps a small LRU of planes.
        """
        self._ensure_plane_tile_set(plane)
        s = self._plane_tile_sets.get(plane)
        return (x, y) in s if s is not None else False

    def _ensure_plane_tile_set(self, plane: int) -> None:
        # Cache hit: refresh LRU order
        if plane in self._plane_tile_sets:
            try:
                self._plane_lru.move_to_end(plane)
            except Exception:
                pass
            return
        # Build the set from DB stream
        tiles: Set[Tuple[int, int]] = set()
        try:
            for row in self._db.iter_tiles_by_plane(plane):
                tiles.add((row.x, row.y))
        except Exception:
            tiles = set()
        self._plane_tile_sets[plane] = tiles
        self._plane_lru[plane] = None
        # Enforce LRU capacity
        if len(self._plane_lru) > self._plane_cache_capacity:
            try:
                evict_plane = next(iter(self._plane_lru))
                self._plane_lru.pop(evict_plane, None)
                self._plane_tile_sets.pop(evict_plane, None)
            except StopIteration:
                pass

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

    # ------------------------------------------------------------------
    # Per-tile touching node LRU caches
    def _get_object_nodes_touching_cached(self, tile: Tile) -> List[ObjectNodeRow]:
        key: Tile = (int(tile[0]), int(tile[1]), int(tile[2]))
        cached = self._obj_touch_cache.get(key)
        if cached is not None:
            try:
                self._obj_touch_lru.move_to_end(key)
            except Exception:
                pass
            return cached
        rows = list(self._db.iter_object_nodes_touching(key))
        self._obj_touch_cache[key] = rows
        self._obj_touch_lru[key] = None
        if len(self._obj_touch_lru) > self._touch_cache_capacity:
            try:
                evict_key = next(iter(self._obj_touch_lru))
                self._obj_touch_lru.pop(evict_key, None)
                self._obj_touch_cache.pop(evict_key, None)
            except StopIteration:
                pass
        return rows

    def _get_npc_nodes_touching_cached(self, tile: Tile) -> List[NpcNodeRow]:
        key: Tile = (int(tile[0]), int(tile[1]), int(tile[2]))
        cached = self._npc_touch_cache.get(key)
        if cached is not None:
            try:
                self._npc_touch_lru.move_to_end(key)
            except Exception:
                pass
            return cached
        rows = list(self._db.iter_npc_nodes_touching(key))
        self._npc_touch_cache[key] = rows
        self._npc_touch_lru[key] = None
        if len(self._npc_touch_lru) > self._touch_cache_capacity:
            try:
                evict_key = next(iter(self._npc_touch_lru))
                self._npc_touch_lru.pop(evict_key, None)
                self._npc_touch_cache.pop(evict_key, None)
            except StopIteration:
                pass
        return rows

    def _build_chain_metadata(self, resolution: NodeChainResolver.resolve.__annotations__.get('return')) -> List[dict]:
        """Convert a ChainResolution into a JSON-friendly chain list.

        Each entry contains: {"type", "id", "cost_ms", "metadata"} with metadata
        tailored to the node type.
        """
        chain: List[dict] = []
        for link in resolution.links:
            ltype = link.ref.type
            meta: dict[str, object] = {}
            row = link.row
            # Attach per-type metadata similar to head edge construction
            if ltype == "object":
                if getattr(row, "action", None) is not None:
                    meta["action"] = row.action
                if getattr(row, "object_id", None) is not None:
                    meta["object_id"] = row.object_id
                if getattr(row, "object_name", None) is not None:
                    meta["object_name"] = row.object_name
                if getattr(row, "match_type", None) is not None:
                    meta["match_type"] = row.match_type
            elif ltype == "ifslot":
                if getattr(row, "interface_id", None) is not None:
                    meta["interface_id"] = row.interface_id
                if getattr(row, "component_id", None) is not None:
                    meta["component_id"] = row.component_id
                if getattr(row, "slot_id", None) is not None:
                    meta["slot_id"] = row.slot_id
                if getattr(row, "click_id", None) is not None:
                    meta["click_id"] = row.click_id
            elif ltype == "npc":
                if getattr(row, "action", None) is not None:
                    meta["action"] = row.action
                if getattr(row, "npc_id", None) is not None:
                    meta["npc_id"] = row.npc_id
                if getattr(row, "npc_name", None) is not None:
                    meta["npc_name"] = row.npc_name
                if getattr(row, "match_type", None) is not None:
                    meta["match_type"] = row.match_type
            elif ltype == "item":
                if getattr(row, "action", None) is not None:
                    meta["action"] = row.action
                if getattr(row, "item_id", None) is not None:
                    meta["item_id"] = row.item_id
            elif ltype == "door":
                if getattr(row, "direction", None) is not None:
                    meta["door_direction"] = row.direction
                if getattr(row, "real_id_open", None) is not None:
                    meta["real_id_open"] = row.real_id_open
                if getattr(row, "real_id_closed", None) is not None:
                    meta["real_id_closed"] = row.real_id_closed
                if getattr(row, "open_action", None) is not None:
                    meta["action"] = row.open_action
            elif ltype == "lodestone":
                if getattr(row, "lodestone", None) is not None:
                    meta["lodestone"] = row.lodestone
            # Include full DB row data for each link
            try:
                meta["db_row"] = asdict(row)
            except Exception:
                pass
            chain.append({
                "type": ltype,
                "id": link.ref.id,
                "cost_ms": link.cost_ms,
                "metadata": meta,
            })
        return chain

    # ------------------------------------------------------------------
    # Chain-head filtering helpers
    def _ensure_chain_head_index(self) -> None:
        if self._chain_head_index_built:
            return
        non_head: Dict[str, Set[int]] = {}

        def add_non_head(t: Optional[str], node_id: Optional[int]) -> None:
            if t is None or node_id is None:
                return
            key = t.strip().lower()
            non_head.setdefault(key, set()).add(int(node_id))

        # Scan all tables for next_node references
        try:
            for row in self._db.iter_door_nodes():
                add_non_head(getattr(row, "next_node_type", None), getattr(row, "next_node_id", None))
        except Exception:
            pass
        try:
            for row in self._db.iter_lodestone_nodes():
                add_non_head(getattr(row, "next_node_type", None), getattr(row, "next_node_id", None))
        except Exception:
            pass
        try:
            for row in self._db.iter_object_nodes():
                add_non_head(getattr(row, "next_node_type", None), getattr(row, "next_node_id", None))
        except Exception:
            pass
        try:
            for row in self._db.iter_ifslot_nodes():
                add_non_head(getattr(row, "next_node_type", None), getattr(row, "next_node_id", None))
        except Exception:
            pass
        try:
            for row in self._db.iter_npc_nodes():
                add_non_head(getattr(row, "next_node_type", None), getattr(row, "next_node_id", None))
        except Exception:
            pass
        try:
            for row in self._db.iter_item_nodes():
                add_non_head(getattr(row, "next_node_type", None), getattr(row, "next_node_id", None))
        except Exception:
            pass

        self._non_head_nodes = non_head
        self._chain_head_index_built = True

    def _is_non_head(self, node_type: str, node_id: int) -> bool:
        return int(node_id) in self._non_head_nodes.get(node_type.strip().lower(), set())

    # ------------------------------------------------------------------
    # Requirements helpers
    def _build_ctx_map(self, options: SearchOptions) -> Dict[str, int]:
        extras = options.extras if hasattr(options, "extras") and isinstance(options.extras, dict) else {}
        # Prefer pre-normalized map from API
        ctx_map = {}
        req_map = extras.get("requirements_map")
        if isinstance(req_map, dict):
            for k, v in req_map.items():
                if isinstance(k, str) and isinstance(v, int):
                    ctx_map[k] = int(v)
        elif isinstance(extras.get("requirements"), list):
            for item in extras["requirements"]:
                if isinstance(item, dict):
                    k = item.get("key")
                    v = item.get("value")
                    if isinstance(k, str) and isinstance(v, int):
                        ctx_map[k] = int(v)
        return ctx_map

    def _fetch_requirement_cached(self, requirement_id: int):
        if requirement_id in self._requirement_cache:
            return self._requirement_cache[requirement_id]
        req = self._db.fetch_requirement(int(requirement_id))
        self._requirement_cache[requirement_id] = req
        return req

    def _passes_requirement(self, requirement_id: Optional[int], ctx_map: Dict[str, int]) -> bool:
        if requirement_id is None:
            return True
        req = self._fetch_requirement_cached(int(requirement_id))
        if req is None:
            self.req_filtered_count += 1
            return False
        ok = evaluate_requirement(req, ctx_map)
        if not ok:
            self.req_filtered_count += 1
        return ok


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
