import os
import sqlite3
from collections import deque

SOURCE_DB_PATH = "/home/query/Dev/tiles.db"  # path to your source SQLite database
OUTPUT_DB_PATH = "worldReachableTiles.db"  # path to the output SQLite database containing reachable tiles
START_TILE = (3200, 3200, 0)  # example starting tile (x, y, plane)
NODE_TABLES = [
    "door_nodes",
    "lodestone_nodes",
    "object_nodes",
    "ifslot_nodes",
    "npc_nodes",
    "item_nodes"
]


# Direction to coordinate delta
DIRS = {
    "north": (0, 1, 0),
    "south": (0, -1, 0),
    "east": (1, 0, 0),
    "west": (-1, 0, 0),
    "northeast": (1, 1, 0),
    "northwest": (-1, 1, 0),
    "southeast": (1, -1, 0),
    "southwest": (-1, -1, 0),
}


def get_neighbors(conn, x, y, plane):
    """Return list of connected tiles from current tile based on allowed_directions."""
    cur = conn.cursor()
    cur.execute(
        "SELECT allowed_directions FROM tiles WHERE x=? AND y=? AND plane=?",
        (x, y, plane),
    )
    row = cur.fetchone()
    if not row or not row[0]:
        return []

    directions = [d.strip().lower() for d in row[0].split(",")]
    neighbors = []

    for d in directions:
        if d in DIRS:
            dx, dy, dp = DIRS[d]
            neighbors.append((x + dx, y + dy, plane + dp))
    return neighbors


def get_door_links(conn):
    """Return list of (inside_tile, outside_tile) tuples from door_nodes."""
    cur = conn.cursor()
    cur.execute(
        """SELECT tile_inside_x, tile_inside_y, tile_inside_plane,
                  tile_outside_x, tile_outside_y, tile_outside_plane
           FROM door_nodes"""
    )
    links = []
    for row in cur.fetchall():
        inside = (row[0], row[1], row[2])
        outside = (row[3], row[4], row[5])
        links.append((inside, outside))
    return links


def get_lodestone_tiles(conn):
    """Return list of all lodestone destination tiles."""
    cur = conn.cursor()
    cur.execute("SELECT dest_x, dest_y, dest_plane FROM lodestone_nodes")
    return [(x, y, p) for x, y, p in cur.fetchall()]


def get_object_transitions(conn):
    """Precompute object-based transitions from orig bounds to dest bounds.

    For each row in `object_nodes` where all orig/dest bounds and planes are present,
    create one-way transitions from every tile in the origin rectangle to every tile
    in the destination rectangle.

    Returns:
        dict[(x, y, plane) -> set[(dx, dy, dplane)]]
    """
    cur = conn.cursor()
    cur.execute(
        """
        SELECT
            orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane,
            dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane
        FROM object_nodes
        """
    )

    transitions = {}
    count_rows = 0
    count_skipped = 0
    count_pairs = 0

    for (
        o_min_x,
        o_max_x,
        o_min_y,
        o_max_y,
        o_plane,
        d_min_x,
        d_max_x,
        d_min_y,
        d_max_y,
        d_plane,
    ) in cur.fetchall():
        count_rows += 1
        # Require complete orig and dest bounds + planes
        if None in (
            o_min_x,
            o_max_x,
            o_min_y,
            o_max_y,
            o_plane,
            d_min_x,
            d_max_x,
            d_min_y,
            d_max_y,
            d_plane,
        ):
            count_skipped += 1
            continue

        # Precompute destination tiles once per row
        dest_tiles = [
            (dx, dy, d_plane)
            for dx in range(d_min_x, d_max_x + 1)
            for dy in range(d_min_y, d_max_y + 1)
        ]

        for ox in range(o_min_x, o_max_x + 1):
            for oy in range(o_min_y, o_max_y + 1):
                origin = (ox, oy, o_plane)
                if origin not in transitions:
                    transitions[origin] = set()
                # Link origin to all destination tiles
                for dt in dest_tiles:
                    transitions[origin].add(dt)
                count_pairs += len(dest_tiles)

    print(
        f"Loaded {count_rows} object nodes: {len(transitions)} origin tiles mapped, "
        f"{count_pairs} total dest links, {count_skipped} skipped due to incomplete bounds"
    )
    return transitions


def get_npc_transitions(conn):
    """Precompute NPC-based transitions from orig bounds to dest bounds.

    Mirrors `get_object_transitions()` but reads from `npc_nodes`.

    Returns:
        dict[(x, y, plane) -> set[(dx, dy, dplane)]]
    """
    cur = conn.cursor()
    cur.execute(
        """
        SELECT
            orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane,
            dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane
        FROM npc_nodes
        """
    )

    transitions = {}
    count_rows = 0
    count_skipped = 0
    count_pairs = 0

    for (
        o_min_x,
        o_max_x,
        o_min_y,
        o_max_y,
        o_plane,
        d_min_x,
        d_max_x,
        d_min_y,
        d_max_y,
        d_plane,
    ) in cur.fetchall():
        count_rows += 1
        if None in (
            o_min_x,
            o_max_x,
            o_min_y,
            o_max_y,
            o_plane,
            d_min_x,
            d_max_x,
            d_min_y,
            d_max_y,
            d_plane,
        ):
            count_skipped += 1
            continue

        dest_tiles = [
            (dx, dy, d_plane)
            for dx in range(d_min_x, d_max_x + 1)
            for dy in range(d_min_y, d_max_y + 1)
        ]

        for ox in range(o_min_x, o_max_x + 1):
            for oy in range(o_min_y, o_max_y + 1):
                origin = (ox, oy, o_plane)
                if origin not in transitions:
                    transitions[origin] = set()
                for dt in dest_tiles:
                    transitions[origin].add(dt)
                count_pairs += len(dest_tiles)

    print(
        f"Loaded {count_rows} npc nodes: {len(transitions)} origin tiles mapped, "
        f"{count_pairs} total dest links, {count_skipped} skipped due to incomplete bounds"
    )
    return transitions


def get_ifslot_dest_tiles(conn):
    """Collect destination tiles from `ifslot_nodes`.

    `ifslot_nodes` lacks origin bounds in the schema; we treat them as UI-triggered
    global transitions that can be performed from anywhere. To avoid repeated work,
    we enqueue these destinations once during BFS.

    Returns:
        list[(x, y, plane)]
    """
    cur = conn.cursor()
    cur.execute(
        """
        SELECT dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane
        FROM ifslot_nodes
        """
    )

    dest_tiles = []
    count_rows = 0
    count_skipped = 0
    for (
        d_min_x,
        d_max_x,
        d_min_y,
        d_max_y,
        d_plane,
    ) in cur.fetchall():
        count_rows += 1
        if None in (d_min_x, d_max_x, d_min_y, d_max_y, d_plane):
            count_skipped += 1
            continue
        for dx in range(d_min_x, d_max_x + 1):
            for dy in range(d_min_y, d_max_y + 1):
                dest_tiles.append((dx, dy, d_plane))

    print(
        f"Loaded {count_rows} ifslot nodes: {len(dest_tiles)} destination tiles, "
        f"{count_skipped} skipped due to incomplete bounds"
    )
    return dest_tiles


def reachable_tiles(conn, start):
    """BFS to find all reachable tiles using allowed directions, doors, lodestones,
    object transitions, npc transitions, and ifslot destinations."""
    queue = deque([start])
    visited = {start}

    door_links = get_door_links(conn)
    lodestones = get_lodestone_tiles(conn)
    object_transitions = get_object_transitions(conn)
    npc_transitions = get_npc_transitions(conn)
    ifslot_dest_tiles = get_ifslot_dest_tiles(conn)

    print(f"Starting BFS from tile {start}")
    print(
        f"Loaded {len(door_links)} door links, {len(lodestones)} lodestones, "
        f"{len(object_transitions)} object-origin tiles, {len(npc_transitions)} npc-origin tiles, "
        f"and {len(ifslot_dest_tiles)} ifslot destination tiles"
    )

    # Enqueue ifslot destinations only once (treated as global UI-triggered teleports)
    ifslot_enqueued = False

    while queue:
        x, y, plane = queue.popleft()

        print(f"Processing tile {(x, y, plane)}")

        # Step 1: Move based on allowed directions
        for nx, ny, np in get_neighbors(conn, x, y, plane):
            if (nx, ny, np) not in visited:
                visited.add((nx, ny, np))
                queue.append((nx, ny, np))
                print(
                    f"Queued neighbor tile {(nx, ny, np)} from {(x, y, plane)}"
                )

        # Step 2: Move through doors
        for a, b in door_links:
            if a == (x, y, plane) and b not in visited:
                visited.add(b)
                queue.append(b)
                print(f"Queued door-linked tile {b} from {a}")
            elif b == (x, y, plane) and a not in visited:
                visited.add(a)
                queue.append(a)
                print(f"Queued door-linked tile {a} from {b}")

        # Step 3: Teleport between all lodestone-connected tiles
        if (x, y, plane) in lodestones:
            for dest in lodestones:
                if dest not in visited:
                    visited.add(dest)
                    queue.append(dest)
                    print(
                        f"Queued lodestone destination tile {dest} from {(x, y, plane)}"
                    )

        # Step 4: Object-based transitions (one-way from origin bounds to destination bounds)
        for dest in object_transitions.get((x, y, plane), ()):  # empty if none
            if dest not in visited:
                visited.add(dest)
                queue.append(dest)
                print(f"Queued object-linked tile {dest} from {(x, y, plane)}")

        # Step 5: NPC-based transitions (one-way from origin bounds to destination bounds)
        for dest in npc_transitions.get((x, y, plane), ()):  # empty if none
            if dest not in visited:
                visited.add(dest)
                queue.append(dest)
                print(f"Queued npc-linked tile {dest} from {(x, y, plane)}")

        # Step 6: IF slot destinations (global, apply once)
        if not ifslot_enqueued and ifslot_dest_tiles:
            enqueued = 0
            for dest in ifslot_dest_tiles:
                if dest not in visited:
                    visited.add(dest)
                    queue.append(dest)
                    enqueued += 1
            ifslot_enqueued = True
            if enqueued:
                print(f"Queued {enqueued} ifslot destination tiles (global) from start context")

    return visited


def write_reachable_tiles(conn, create_tiles_sql, tile_columns, tile_rows):
    """Create a clean tiles table and insert reachable tile rows."""
    cur = conn.cursor()

    print("Creating tiles table in output database")
    cur.execute(create_tiles_sql)

    placeholders = ", ".join("?" for _ in tile_columns)
    columns_clause = ", ".join(tile_columns)
    insert_sql = f"INSERT INTO tiles ({columns_clause}) VALUES ({placeholders})"

    print(f"Inserting {len(tile_rows)} reachable tiles into output database")
    cur.executemany(insert_sql, tile_rows)

    conn.commit()
    print(f"Committed {len(tile_rows)} reachable tiles to output database")


def copy_tables(source_conn, dest_conn, tables):
    """Copy table schemas and contents from source_conn into dest_conn."""
    source_cur = source_conn.cursor()
    dest_cur = dest_conn.cursor()

    for table in tables:
        print(f"Copying table {table} into output database")

        create_row = source_cur.execute(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name=?",
            (table,),
        ).fetchone()
        if not create_row or not create_row[0]:
            print(f"Warning: Could not retrieve CREATE TABLE statement for {table}")
            continue

        dest_cur.execute(create_row[0])

        column_info = source_cur.execute(f"PRAGMA table_info({table})").fetchall()
        columns = [row[1] for row in column_info]
        if not columns:
            print(f"Warning: No columns found for {table}; skipping data copy")
            continue

        column_clause = ", ".join(columns)
        placeholders = ", ".join("?" for _ in columns)
        select_sql = f"SELECT {column_clause} FROM {table}"
        rows = source_cur.execute(select_sql).fetchall()
        if rows:
            insert_sql = f"INSERT INTO {table} ({column_clause}) VALUES ({placeholders})"
            dest_cur.executemany(
                insert_sql,
                [tuple(row[col] for col in columns) for row in rows],
            )
            print(f"Inserted {len(rows)} rows into {table}")
        else:
            print(f"No rows found in {table}; created empty table")

    dest_conn.commit()
    print("Committed copied node tables to output database")


def main():
    print(f"Connecting to source database at {SOURCE_DB_PATH}")
    source_conn = sqlite3.connect(SOURCE_DB_PATH)
    source_conn.row_factory = sqlite3.Row
    source_cur = source_conn.cursor()

    reachable = reachable_tiles(source_conn, START_TILE)
    print(f"Reachable tiles found: {len(reachable)}")

    create_tiles_sql_row = source_cur.execute(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='tiles'"
    ).fetchone()
    if not create_tiles_sql_row or not create_tiles_sql_row[0]:
        raise RuntimeError("Could not retrieve CREATE TABLE statement for tiles")
    create_tiles_sql = create_tiles_sql_row[0]

    tile_info_rows = source_cur.execute("PRAGMA table_info(tiles)").fetchall()
    tile_columns = [row[1] for row in tile_info_rows]
    if not tile_columns:
        raise RuntimeError("No columns found for tiles table")

    print(f"Tiles table columns: {tile_columns}")

    tile_rows = []
    missing_tiles = []
    for tile in reachable:
        source_cur.execute(
            "SELECT * FROM tiles WHERE x=? AND y=? AND plane=?",
            tile,
        )
        row = source_cur.fetchone()
        if row is None:
            missing_tiles.append(tile)
            continue
        tile_rows.append(tuple(row[col] for col in tile_columns))

    if missing_tiles:
        print(f"Warning: {len(missing_tiles)} reachable tiles missing from source tiles table")

    print(f"Collected {len(tile_rows)} tile rows to copy into output database")

    if os.path.exists(OUTPUT_DB_PATH):
        print(f"Removing existing output database at {OUTPUT_DB_PATH}")
        os.remove(OUTPUT_DB_PATH)

    print(f"Creating new output database at {OUTPUT_DB_PATH}")
    output_conn = sqlite3.connect(OUTPUT_DB_PATH)
    write_reachable_tiles(output_conn, create_tiles_sql, tile_columns, tile_rows)
    copy_tables(source_conn, output_conn, NODE_TABLES)
    output_conn.close()
    print("Output database connection closed")

    source_conn.close()
    print("Source database connection closed")


if __name__ == "__main__":
    main()
