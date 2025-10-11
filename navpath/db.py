"""SQLite access helpers for the navpath service.

This module centralises creation of read-only SQLite connections and
provides typed accessors over the tables described in
`docs/tiles_nodes_schema.md`. All queries are parameterised and can be
re-used safely by higher-level components without risking accidental
writes.
"""

from __future__ import annotations

import sqlite3
from dataclasses import dataclass, field
from pathlib import Path
from typing import Dict, Iterable, Iterator, Optional, Tuple, Union

from .path import Tile

SqlConnection = sqlite3.Connection


def _coerce_path(path: Union[str, Path]) -> str:
    return str(Path(path))


def open_connection(db_path: Union[str, Path]) -> SqlConnection:
    """Return a SQLite connection opened in read-only mode when possible.

    The returned connection uses ``sqlite3.Row`` for ``row_factory`` so
    that column access by name is available to all downstream helpers.
    If read-only mode is unavailable (e.g., older SQLite builds), the
    function falls back to a normal connection while enabling foreign
    keys and keeping autocommit semantics to discourage writes.
    """

    path = _coerce_path(db_path)
    uri = f"file:{Path(path).absolute()}?mode=ro"
    try:
        conn = sqlite3.connect(uri, uri=True, check_same_thread=False)
    except sqlite3.OperationalError:
        conn = sqlite3.connect(path, check_same_thread=False)
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA foreign_keys = ON")
    conn.isolation_level = None  # autocommit; upstream code should not write
    return conn


@dataclass(slots=True)
class TileRow:
    """Represents a tile entry from the ``tiles`` table."""

    x: int
    y: int
    plane: int
    tiledata: Optional[int]
    allowed_directions: Optional[int]
    blocked_directions: Optional[int]


@dataclass(slots=True)
class DoorNodeRow:
    """Typed view across the columns used from ``door_nodes``."""

    id: int
    direction: Optional[str]
    tile_inside: Tile
    tile_outside: Tile
    location_open: Tile
    location_closed: Tile
    real_id_open: int
    real_id_closed: int
    cost: Optional[int]
    open_action: Optional[str]
    next_node_type: Optional[str]
    next_node_id: Optional[int]
    requirement_id: Optional[int]


@dataclass(slots=True)
class LodestoneNodeRow:
    """Typed view of ``lodestone_nodes``."""

    id: int
    lodestone: str
    dest: Tile
    cost: Optional[int]
    next_node_type: Optional[str]
    next_node_id: Optional[int]
    requirement_id: Optional[int]


@dataclass(slots=True)
class ObjectNodeRow:
    """Typed view of ``object_nodes``."""

    id: int
    match_type: str
    object_id: Optional[int]
    object_name: Optional[str]
    action: Optional[str]
    dest_min_x: Optional[int]
    dest_max_x: Optional[int]
    dest_min_y: Optional[int]
    dest_max_y: Optional[int]
    dest_plane: Optional[int]
    orig_min_x: Optional[int]
    orig_max_x: Optional[int]
    orig_min_y: Optional[int]
    orig_max_y: Optional[int]
    orig_plane: Optional[int]
    search_radius: int
    cost: Optional[int]
    next_node_type: Optional[str]
    next_node_id: Optional[int]
    requirement_id: Optional[int]


@dataclass(slots=True)
class IfslotNodeRow:
    """Typed view of ``ifslot_nodes``."""

    id: int
    interface_id: int
    component_id: int
    slot_id: Optional[int]
    click_id: int
    dest_min_x: Optional[int]
    dest_max_x: Optional[int]
    dest_min_y: Optional[int]
    dest_max_y: Optional[int]
    dest_plane: Optional[int]
    cost: Optional[int]
    next_node_type: Optional[str]
    next_node_id: Optional[int]
    requirement_id: Optional[int]


@dataclass(slots=True)
class NpcNodeRow:
    """Typed view of ``npc_nodes``."""

    id: int
    match_type: str
    npc_id: Optional[int]
    npc_name: Optional[str]
    action: Optional[str]
    dest_min_x: Optional[int]
    dest_max_x: Optional[int]
    dest_min_y: Optional[int]
    dest_max_y: Optional[int]
    dest_plane: Optional[int]
    orig_min_x: Optional[int]
    orig_max_x: Optional[int]
    orig_min_y: Optional[int]
    orig_max_y: Optional[int]
    orig_plane: Optional[int]
    search_radius: int
    cost: Optional[int]
    next_node_type: Optional[str]
    next_node_id: Optional[int]
    requirement_id: Optional[int]


@dataclass(slots=True)
class ItemNodeRow:
    """Typed view of ``item_nodes``."""

    id: int
    item_id: Optional[int]
    action: Optional[str]
    dest_min_x: Optional[int]
    dest_max_x: Optional[int]
    dest_min_y: Optional[int]
    dest_max_y: Optional[int]
    dest_plane: Optional[int]
    cost: Optional[int]
    next_node_type: Optional[str]
    next_node_id: Optional[int]
    requirement_id: Optional[int]


@dataclass(slots=True)
class RequirementRow:
    """Typed view of ``requirements``."""

    id: int
    metaInfo: Optional[str]
    key: Optional[str]
    value: Optional[int]
    comparison: Optional[str]


NodeRow = Union[
    DoorNodeRow,
    LodestoneNodeRow,
    ObjectNodeRow,
    IfslotNodeRow,
    NpcNodeRow,
    ItemNodeRow,
]


@dataclass(slots=True)
class Database:
    """High-level helper that exposes prepared lookups over navpath tables."""

    connection: SqlConnection = field(repr=False)

    _sql_tile_by_coord: str = field(init=False, default=(
        "SELECT x, y, plane, tiledata, allowed_directions, blocked_directions FROM tiles "
        "WHERE x = ? AND y = ? AND plane = ?"
    ))
    _sql_distinct_planes: str = field(init=False, default=(
        "SELECT DISTINCT plane FROM tiles ORDER BY plane ASC"
    ))
    _sql_tiles_by_plane: str = field(init=False, default=(
        "SELECT x, y, plane, tiledata, allowed_directions, blocked_directions FROM tiles "
        "WHERE plane = ? ORDER BY y ASC, x ASC"
    ))
    _sql_door_by_id: str = field(init=False, default=(
        "SELECT id, direction, tile_inside_x, tile_inside_y, tile_inside_plane, "
        "tile_outside_x, tile_outside_y, tile_outside_plane, "
        "location_open_x, location_open_y, location_open_plane, "
        "location_closed_x, location_closed_y, location_closed_plane, "
        "real_id_open, real_id_closed, cost, open_action, next_node_type, next_node_id, requirement_id "
        "FROM door_nodes WHERE id = ?"
    ))
    _sql_door_by_tile: str = field(init=False, default=(
        "SELECT id, direction, tile_inside_x, tile_inside_y, tile_inside_plane, "
        "tile_outside_x, tile_outside_y, tile_outside_plane, "
        "location_open_x, location_open_y, location_open_plane, "
        "location_closed_x, location_closed_y, location_closed_plane, "
        "real_id_open, real_id_closed, cost, open_action, next_node_type, next_node_id, requirement_id "
        "FROM door_nodes WHERE (tile_inside_x = ? AND tile_inside_y = ? AND tile_inside_plane = ?) "
        "OR (tile_outside_x = ? AND tile_outside_y = ? AND tile_outside_plane = ?)"
    ))
    _sql_all_doors: str = field(init=False, default=(
        "SELECT id, direction, tile_inside_x, tile_inside_y, tile_inside_plane, "
        "tile_outside_x, tile_outside_y, tile_outside_plane, "
        "location_open_x, location_open_y, location_open_plane, "
        "location_closed_x, location_closed_y, location_closed_plane, "
        "real_id_open, real_id_closed, cost, open_action, next_node_type, next_node_id, requirement_id "
        "FROM door_nodes"
    ))
    _sql_lodestone_by_id: str = field(init=False, default=(
        "SELECT id, lodestone, dest_x, dest_y, dest_plane, cost, next_node_type, next_node_id, requirement_id "
        "FROM lodestone_nodes WHERE id = ?"
    ))
    _sql_all_lodestones: str = field(init=False, default=(
        "SELECT id, lodestone, dest_x, dest_y, dest_plane, cost, next_node_type, next_node_id, requirement_id "
        "FROM lodestone_nodes"
    ))
    _sql_object_by_id: str = field(init=False, default=(
        "SELECT id, match_type, object_id, object_name, action, "
        "dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane, "
        "orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane, "
        "search_radius, cost, next_node_type, next_node_id, requirement_id "
        "FROM object_nodes WHERE id = ?"
    ))
    _sql_ifslot_by_id: str = field(init=False, default=(
        "SELECT id, interface_id, component_id, slot_id, click_id, "
        "dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane, cost, next_node_type, next_node_id, requirement_id "
        "FROM ifslot_nodes WHERE id = ?"
    ))
    _sql_npc_by_id: str = field(init=False, default=(
        "SELECT id, match_type, npc_id, npc_name, action, dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane, "
        "orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane, search_radius, cost, next_node_type, next_node_id, requirement_id "
        "FROM npc_nodes WHERE id = ?"
    ))
    _sql_item_by_id: str = field(init=False, default=(
        "SELECT id, item_id, action, dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane, cost, next_node_type, next_node_id, requirement_id "
        "FROM item_nodes WHERE id = ?"
    ))
    _sql_all_ifslots: str = field(init=False, default=(
        "SELECT id, interface_id, component_id, slot_id, click_id, "
        "dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane, cost, next_node_type, next_node_id, requirement_id "
        "FROM ifslot_nodes"
    ))
    _sql_all_items: str = field(init=False, default=(
        "SELECT id, item_id, action, dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane, cost, next_node_type, next_node_id, requirement_id "
        "FROM item_nodes"
    ))
    _sql_all_objects: str = field(init=False, default=(
        "SELECT id, match_type, object_id, object_name, action, "
        "dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane, "
        "orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane, "
        "search_radius, cost, next_node_type, next_node_id, requirement_id "
        "FROM object_nodes"
    ))
    _sql_all_npcs: str = field(init=False, default=(
        "SELECT id, match_type, npc_id, npc_name, action, dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane, "
        "orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane, search_radius, cost, next_node_type, next_node_id, requirement_id "
        "FROM npc_nodes"
    ))
    _sql_object_by_origin_tile: str = field(init=False, default=(
        "SELECT id, match_type, object_id, object_name, action, "
        "dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane, "
        "orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane, "
        "search_radius, cost, next_node_type, next_node_id, requirement_id "
        "FROM object_nodes "
        "WHERE (" 
        "  orig_min_x IS NOT NULL AND orig_max_x IS NOT NULL "
        "  AND orig_min_y IS NOT NULL AND orig_max_y IS NOT NULL "
        "  AND ? BETWEEN orig_min_x AND orig_max_x "
        "  AND ? BETWEEN orig_min_y AND orig_max_y "
        "  AND (orig_plane IS NULL OR orig_plane = ?)"
        ") OR ("
        "  orig_min_x IS NULL OR orig_max_x IS NULL "
        "  OR orig_min_y IS NULL OR orig_max_y IS NULL"
        ")"
    ))
    _sql_npc_by_origin_tile: str = field(init=False, default=(
        "SELECT id, match_type, npc_id, npc_name, action, dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane, "
        "orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane, search_radius, cost, next_node_type, next_node_id, requirement_id "
        "FROM npc_nodes "
        "WHERE ("
        "  orig_min_x IS NOT NULL AND orig_max_x IS NOT NULL "
        "  AND orig_min_y IS NOT NULL AND orig_max_y IS NOT NULL "
        "  AND ? BETWEEN orig_min_x AND orig_max_x "
        "  AND ? BETWEEN orig_min_y AND orig_max_y "
        "  AND (orig_plane IS NULL OR orig_plane = ?)"
        ") OR ("
        "  orig_min_x IS NULL OR orig_max_x IS NULL "
        "  OR orig_min_y IS NULL OR orig_max_y IS NULL"
        ")"
    ))

    _sql_requirement_by_id: str = field(init=False, default=(
        "SELECT id, metaInfo, key, value, comparison FROM requirements WHERE id = ?"
    ))

    @classmethod
    def connect(cls, db_path: Union[str, Path]) -> "Database":
        """Create a :class:`Database` bound to ``db_path``."""

        return cls(open_connection(db_path))

    def close(self) -> None:
        """Close the underlying SQLite connection."""

        self.connection.close()

    # -- tile helpers -------------------------------------------------
    def fetch_tile(self, x: int, y: int, plane: int) -> Optional[TileRow]:
        """Return the tile at the given coordinate, if present."""

        row = self.connection.execute(self._sql_tile_by_coord, (x, y, plane)).fetchone()
        if row is None:
            return None
        return TileRow(
            x=row["x"],
            y=row["y"],
            plane=row["plane"],
            tiledata=row["tiledata"],
            allowed_directions=row["allowed_directions"],
            blocked_directions=row["blocked_directions"],
        )

    # -- tile streaming helpers --------------------------------------
    def iter_planes(self) -> Iterator[int]:
        """Yield distinct plane identifiers in ascending order."""

        cursor = self.connection.execute(self._sql_distinct_planes)
        for row in cursor:
            yield int(row[0])

    def iter_tiles_by_plane(self, plane: int) -> Iterator[TileRow]:
        """Yield tiles on ``plane`` ordered by (y ASC, x ASC)."""

        cursor = self.connection.execute(self._sql_tiles_by_plane, (plane,))
        for row in cursor:
            yield TileRow(
                x=row["x"],
                y=row["y"],
                plane=row["plane"],
                tiledata=row["tiledata"],
                allowed_directions=row["allowed_directions"],
                blocked_directions=row["blocked_directions"],
            )

    def iter_object_nodes_touching(self, tile: Tile) -> Iterator[ObjectNodeRow]:
        """Yield object nodes whose origin bounds include ``tile``.

        Also yields nodes with no origin bounds defined (any of
        ``orig_min_x``, ``orig_max_x``, ``orig_min_y``, ``orig_max_y``
        is NULL), treating them as usable from any tile.
        """

        x, y, plane = tile
        cursor = self.connection.execute(self._sql_object_by_origin_tile, (x, y, plane))
        for row in cursor:
            yield ObjectNodeRow(
                id=row["id"],
                match_type=row["match_type"],
                object_id=row["object_id"],
                object_name=row["object_name"],
                action=row["action"],
                dest_min_x=row["dest_min_x"],
                dest_max_x=row["dest_max_x"],
                dest_min_y=row["dest_min_y"],
                dest_max_y=row["dest_max_y"],
                dest_plane=row["dest_plane"],
                orig_min_x=row["orig_min_x"],
                orig_max_x=row["orig_max_x"],
                orig_min_y=row["orig_min_y"],
                orig_max_y=row["orig_max_y"],
                orig_plane=row["orig_plane"],
                search_radius=row["search_radius"],
                cost=row["cost"],
                next_node_type=row["next_node_type"],
                next_node_id=row["next_node_id"],
                requirement_id=row["requirement_id"],
            )

    # -- door helpers -------------------------------------------------
    def fetch_door_node(self, node_id: int) -> Optional[DoorNodeRow]:
        """Return a door node by primary key."""

        row = self.connection.execute(self._sql_door_by_id, (node_id,)).fetchone()
        if row is None:
            return None
        return _make_door_node(row)

    def iter_door_nodes_touching(self, tile: Tile) -> Iterator[DoorNodeRow]:
        """Yield door nodes whose inside/outside tiles include ``tile``."""

        params = (*tile, *tile)
        cursor = self.connection.execute(self._sql_door_by_tile, params)
        for row in cursor:
            yield _make_door_node(row)

    def iter_door_nodes(self) -> Iterator[DoorNodeRow]:
        """Yield all door nodes."""

        cursor = self.connection.execute(self._sql_all_doors)
        for row in cursor:
            yield _make_door_node(row)

    # -- lodestone helpers --------------------------------------------
    def fetch_lodestone_node(self, node_id: int) -> Optional[LodestoneNodeRow]:
        """Return a lodestone node by primary key."""

        row = self.connection.execute(self._sql_lodestone_by_id, (node_id,)).fetchone()
        if row is None:
            return None
        return LodestoneNodeRow(
            id=row["id"],
            lodestone=row["lodestone"],
            dest=(row["dest_x"], row["dest_y"], row["dest_plane"]),
            cost=row["cost"],
            next_node_type=row["next_node_type"],
            next_node_id=row["next_node_id"],
            requirement_id=row["requirement_id"],
        )

    def iter_lodestone_nodes(self) -> Iterator[LodestoneNodeRow]:
        """Yield all lodestone nodes.

        The lodestone table is typically small (limited to the number of
        teleport destinations). Loading all rows eagerly per search is
        acceptable and avoids per-query joins in the graph layer.
        """

        cursor = self.connection.execute(self._sql_all_lodestones)
        for row in cursor:
            yield LodestoneNodeRow(
                id=row["id"],
                lodestone=row["lodestone"],
                dest=(row["dest_x"], row["dest_y"], row["dest_plane"]),
                cost=row["cost"],
                next_node_type=row["next_node_type"],
                next_node_id=row["next_node_id"],
                requirement_id=row["requirement_id"],
            )

    # -- object helpers -----------------------------------------------
    def fetch_object_node(self, node_id: int) -> Optional[ObjectNodeRow]:
        """Return an object node by primary key."""

        row = self.connection.execute(self._sql_object_by_id, (node_id,)).fetchone()
        if row is None:
            return None
        return ObjectNodeRow(
            id=row["id"],
            match_type=row["match_type"],
            object_id=row["object_id"],
            object_name=row["object_name"],
            action=row["action"],
            dest_min_x=row["dest_min_x"],
            dest_max_x=row["dest_max_x"],
            dest_min_y=row["dest_min_y"],
            dest_max_y=row["dest_max_y"],
            dest_plane=row["dest_plane"],
            orig_min_x=row["orig_min_x"],
            orig_max_x=row["orig_max_x"],
            orig_min_y=row["orig_min_y"],
            orig_max_y=row["orig_max_y"],
            orig_plane=row["orig_plane"],
            search_radius=row["search_radius"],
            cost=row["cost"],
            next_node_type=row["next_node_type"],
            next_node_id=row["next_node_id"],
            requirement_id=row["requirement_id"],
        )

    def iter_object_nodes(self) -> Iterator[ObjectNodeRow]:
        """Yield all object nodes (full table scan)."""

        cursor = self.connection.execute(self._sql_all_objects)
        for row in cursor:
            yield ObjectNodeRow(
                id=row["id"],
                match_type=row["match_type"],
                object_id=row["object_id"],
                object_name=row["object_name"],
                action=row["action"],
                dest_min_x=row["dest_min_x"],
                dest_max_x=row["dest_max_x"],
                dest_min_y=row["dest_min_y"],
                dest_max_y=row["dest_max_y"],
                dest_plane=row["dest_plane"],
                orig_min_x=row["orig_min_x"],
                orig_max_x=row["orig_max_x"],
                orig_min_y=row["orig_min_y"],
                orig_max_y=row["orig_max_y"],
                orig_plane=row["orig_plane"],
                search_radius=row["search_radius"],
                cost=row["cost"],
                next_node_type=row["next_node_type"],
                next_node_id=row["next_node_id"],
                requirement_id=row["requirement_id"],
            )

    # -- ifslot helpers -----------------------------------------------
    def fetch_ifslot_node(self, node_id: int) -> Optional[IfslotNodeRow]:
        """Return an ifslot node by primary key."""

        row = self.connection.execute(self._sql_ifslot_by_id, (node_id,)).fetchone()
        if row is None:
            return None
        return IfslotNodeRow(
            id=row["id"],
            interface_id=row["interface_id"],
            component_id=row["component_id"],
            slot_id=row["slot_id"],
            click_id=row["click_id"],
            dest_min_x=row["dest_min_x"],
            dest_max_x=row["dest_max_x"],
            dest_min_y=row["dest_min_y"],
            dest_max_y=row["dest_max_y"],
            dest_plane=row["dest_plane"],
            cost=row["cost"],
            next_node_type=row["next_node_type"],
            next_node_id=row["next_node_id"],
            requirement_id=row["requirement_id"],
        )

    def iter_ifslot_nodes(self) -> Iterator[IfslotNodeRow]:
        """Yield all ifslot nodes (table is typically small)."""

        cursor = self.connection.execute(self._sql_all_ifslots)
        for row in cursor:
            yield IfslotNodeRow(
                id=row["id"],
                interface_id=row["interface_id"],
                component_id=row["component_id"],
                slot_id=row["slot_id"],
                click_id=row["click_id"],
                dest_min_x=row["dest_min_x"],
                dest_max_x=row["dest_max_x"],
                dest_min_y=row["dest_min_y"],
                dest_max_y=row["dest_max_y"],
                dest_plane=row["dest_plane"],
                cost=row["cost"],
                next_node_type=row["next_node_type"],
                next_node_id=row["next_node_id"],
                requirement_id=row["requirement_id"],
            )

    # -- npc helpers --------------------------------------------------
    def fetch_npc_node(self, node_id: int) -> Optional[NpcNodeRow]:
        """Return an NPC node by primary key."""

        row = self.connection.execute(self._sql_npc_by_id, (node_id,)).fetchone()
        if row is None:
            return None
        return NpcNodeRow(
            id=row["id"],
            match_type=row["match_type"],
            npc_id=row["npc_id"],
            npc_name=row["npc_name"],
            action=row["action"],
            dest_min_x=row["dest_min_x"],
            dest_max_x=row["dest_max_x"],
            dest_min_y=row["dest_min_y"],
            dest_max_y=row["dest_max_y"],
            dest_plane=row["dest_plane"],
            orig_min_x=row["orig_min_x"],
            orig_max_x=row["orig_max_x"],
            orig_min_y=row["orig_min_y"],
            orig_max_y=row["orig_max_y"],
            orig_plane=row["orig_plane"],
            search_radius=row["search_radius"],
            cost=row["cost"],
            next_node_type=row["next_node_type"],
            next_node_id=row["next_node_id"],
            requirement_id=row["requirement_id"],
        )

    def iter_npc_nodes(self) -> Iterator[NpcNodeRow]:
        """Yield all npc nodes (full table scan)."""

        cursor = self.connection.execute(self._sql_all_npcs)
        for row in cursor:
            yield NpcNodeRow(
                id=row["id"],
                match_type=row["match_type"],
                npc_id=row["npc_id"],
                npc_name=row["npc_name"],
                action=row["action"],
                dest_min_x=row["dest_min_x"],
                dest_max_x=row["dest_max_x"],
                dest_min_y=row["dest_min_y"],
                dest_max_y=row["dest_max_y"],
                dest_plane=row["dest_plane"],
                orig_min_x=row["orig_min_x"],
                orig_max_x=row["orig_max_x"],
                orig_min_y=row["orig_min_y"],
                orig_max_y=row["orig_max_y"],
                orig_plane=row["orig_plane"],
                search_radius=row["search_radius"],
                cost=row["cost"],
                next_node_type=row["next_node_type"],
                next_node_id=row["next_node_id"],
                requirement_id=row["requirement_id"],
            )

    def iter_npc_nodes_touching(self, tile: Tile) -> Iterator[NpcNodeRow]:
        """Yield NPC nodes whose origin bounds include ``tile``.

        Also yields nodes with no origin bounds defined (any of
        ``orig_min_x``, ``orig_max_x``, ``orig_min_y``, ``orig_max_y``
        is NULL), treating them as usable from any tile.
        """

        x, y, plane = tile
        cursor = self.connection.execute(self._sql_npc_by_origin_tile, (x, y, plane))
        for row in cursor:
            yield NpcNodeRow(
                id=row["id"],
                match_type=row["match_type"],
                npc_id=row["npc_id"],
                npc_name=row["npc_name"],
                action=row["action"],
                dest_min_x=row["dest_min_x"],
                dest_max_x=row["dest_max_x"],
                dest_min_y=row["dest_min_y"],
                dest_max_y=row["dest_max_y"],
                dest_plane=row["dest_plane"],
                orig_min_x=row["orig_min_x"],
                orig_max_x=row["orig_max_x"],
                orig_min_y=row["orig_min_y"],
                orig_max_y=row["orig_max_y"],
                orig_plane=row["orig_plane"],
                search_radius=row["search_radius"],
                cost=row["cost"],
                next_node_type=row["next_node_type"],
                next_node_id=row["next_node_id"],
                requirement_id=row["requirement_id"],
            )

    # -- item helpers -------------------------------------------------
    def fetch_item_node(self, node_id: int) -> Optional[ItemNodeRow]:
        """Return an item node by primary key."""

        row = self.connection.execute(self._sql_item_by_id, (node_id,)).fetchone()
        if row is None:
            return None
        return ItemNodeRow(
            id=row["id"],
            item_id=row["item_id"],
            action=row["action"],
            dest_min_x=row["dest_min_x"],
            dest_max_x=row["dest_max_x"],
            dest_min_y=row["dest_min_y"],
            dest_max_y=row["dest_max_y"],
            dest_plane=row["dest_plane"],
            cost=row["cost"],
            next_node_type=row["next_node_type"],
            next_node_id=row["next_node_id"],
            requirement_id=row["requirement_id"],
        )

    def iter_item_nodes(self) -> Iterator[ItemNodeRow]:
        """Yield all item nodes (table is typically small)."""

        cursor = self.connection.execute(self._sql_all_items)
        for row in cursor:
            yield ItemNodeRow(
                id=row["id"],
                item_id=row["item_id"],
                action=row["action"],
                dest_min_x=row["dest_min_x"],
                dest_max_x=row["dest_max_x"],
                dest_min_y=row["dest_min_y"],
                dest_max_y=row["dest_max_y"],
                dest_plane=row["dest_plane"],
                cost=row["cost"],
                next_node_type=row["next_node_type"],
                next_node_id=row["next_node_id"],
                requirement_id=row["requirement_id"],
            )

    # -- requirements helpers ----------------------------------------
    def fetch_requirement(self, requirement_id: int) -> Optional[RequirementRow]:
        """Return a requirement row by primary key from ``requirements``.

        This is read-only and parameterized.
        """

        row = self.connection.execute(self._sql_requirement_by_id, (requirement_id,)).fetchone()
        if row is None:
            return None
        return RequirementRow(
            id=row["id"],
            metaInfo=row["metaInfo"],
            key=row["key"],
            value=row["value"],
            comparison=row["comparison"],
        )

    # -- generic dispatch --------------------------------------------
    def fetch_node(self, node_type: str, node_id: int) -> Optional[NodeRow]:
        """Return a node row based on ``node_type`` keyword.

        Supported ``node_type`` values are ``door``, ``lodestone``,
        ``object``, ``ifslot``, ``npc``, and ``item``.
        """

        node_type = node_type.lower()
        if node_type == "door":
            return self.fetch_door_node(node_id)
        if node_type == "lodestone":
            return self.fetch_lodestone_node(node_id)
        if node_type == "object":
            return self.fetch_object_node(node_id)
        if node_type == "ifslot":
            return self.fetch_ifslot_node(node_id)
        if node_type == "npc":
            return self.fetch_npc_node(node_id)
        if node_type == "item":
            return self.fetch_item_node(node_id)
        raise ValueError(f"Unknown node_type: {node_type}")


def _make_door_node(row: sqlite3.Row) -> DoorNodeRow:
    inside: Tile = (row["tile_inside_x"], row["tile_inside_y"], row["tile_inside_plane"])
    outside: Tile = (row["tile_outside_x"], row["tile_outside_y"], row["tile_outside_plane"])
    open_location: Tile = (row["location_open_x"], row["location_open_y"], row["location_open_plane"])
    closed_location: Tile = (
        row["location_closed_x"],
        row["location_closed_y"],
        row["location_closed_plane"],
    )
    return DoorNodeRow(
        id=row["id"],
        direction=row["direction"],
        tile_inside=inside,
        tile_outside=outside,
        location_open=open_location,
        location_closed=closed_location,
        real_id_open=row["real_id_open"],
        real_id_closed=row["real_id_closed"],
        cost=row["cost"],
        open_action=row["open_action"],
        next_node_type=row["next_node_type"],
        next_node_id=row["next_node_id"],
        requirement_id=row["requirement_id"],
    )
