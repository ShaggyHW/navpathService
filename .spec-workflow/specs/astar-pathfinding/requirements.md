# Requirements Document

## Introduction
Implement an A* pathfinding service that returns the least-cost path between a start and end tile using the world graph stored in the SQLite database `worldReachableTiles.db`. The graph consists of:
- Tile adjacency from `tiles.allowed_directions` (8-directional).
- Bidirectional door links from `door_nodes` (inside <-> outside).
- Teleport links between lodestones from `lodestone_nodes`.

Note: The database schema includes additional node tables (`object_nodes`, `ifslot_nodes`, `npc_nodes`, `item_nodes`) and a `next_node_type` field across node tables that now also permits the value `'item'` (see `docs/tiles_nodes_schema.md`). This spec SHALL leverage all these node tables for traversal edges where applicable and SHALL honor `next_node_type/next_node_id` chaining as mandatory execution when present (forming a composed action sequence that contributes a single transition in the search graph with accumulated cost).

The output includes:
- `path`: a sequence of tiles `(x, y, plane)` from start to goal inclusive, or `None` when unreachable.
- `actions`: an ordered list of action steps (movement and node interactions) required to traverse the route; empty when `start == goal` or when no path exists.

## Alignment with Product Vision
- Provide a reliable, fast, deterministic pathfinding core for navigation features.
- Reuse existing SQLite world data (`tiles`, `door_nodes`, `lodestone_nodes`) to avoid duplicate sources of truth.
- Foundation for higher-level route planning and actions execution.

## Requirements

### Requirement 1: Compute path with A*
**User Story:** As a navigation client, I want to compute the optimal path between two tiles so that I can traverse the world efficiently and deterministically.

#### Acceptance Criteria
1. WHEN given a valid `start=(x,y,plane)` and `goal=(x,y,plane)` that are connected, THEN the system SHALL return both:
   - `path`: list of `(x,y,plane)` including both endpoints, and
   - `actions`: an ordered list of steps that, when executed in order, traverse the route.
2. IF `start == goal` THEN the system SHALL return `path=[start]` and `actions=[]`.
3. IF no path exists, THEN the system SHALL return `path=None`, `actions=[]`, and a `reason` string (e.g., "unreachable").
4. The algorithm SHALL be A* with an admissible, consistent heuristic for 8-directional movement (octile distance) to ensure optimality.
5. The implementation SHALL be deterministic given identical inputs and configuration (including tie-breaking).
6. The `actions` list SHALL include step types: `move`, `door`, `lodestone`, `object`, `ifslot`, `npc`, `item`. Each step SHALL include: `from:(x,y,plane)`, `to:(x,y,plane)`, `cost_ms:int`, and when applicable `node:{type,id}` referencing the originating node.

### Requirement 2: Graph modeling from database
**User Story:** As a developer, I want the graph derived from the SQLite DB so that the pathfinder always reflects the current world data.

#### Acceptance Criteria
1. The system SHALL read neighbors from `tiles.allowed_directions` in table `tiles(x,y,plane,...)`.
   - Cardinal moves: north, south, east, west
   - Diagonals: northeast, northwest, southeast, southwest
2. The system SHALL add bidirectional edges from `door_nodes` linking `(tile_inside_x, tile_inside_y, tile_inside_plane)` and `(tile_outside_x, tile_outside_y, tile_outside_plane)`.
3. The system SHALL add teleport edges between any lodestone tile and every other lodestone tile derived from `lodestone_nodes(lodestone, dest_x, dest_y, dest_plane)`.
4. The system SHALL validate that both start and goal exist in `tiles`. If either does not exist, return an error result (no path, reason="tile-not-found").
5. The system SHALL add action edges from `object_nodes` when a node specifies destination bounds (`dest_*`/`dest_plane`). The edge source is any tile within the optional origin bounds (`orig_*`/`orig_plane`) when provided; otherwise the source is the current tile. The edge target set is all tiles within destination bounds (treated as reachable via a single action). If both origin and destination are bounds, treat the action as a portal from any origin tile to any destination tile.
6. The system SHALL add action edges from `ifslot_nodes` to the destination bounds (`dest_*`/`dest_plane`) when present. If no bounds are provided, the node does not contribute an edge.
7. The system SHALL add action edges from `npc_nodes` similar to `object_nodes`. The edge source is any tile within the optional origin bounds; the target is all tiles within destination bounds when present.
8. The system SHALL add action edges from `item_nodes` to destination bounds when present.
9. The system SHALL implement `next_node_type/next_node_id` chaining as mandatory execution: executing a node immediately executes its `next_node` (and subsequent chains) before resuming normal movement. In the search graph, model the entire chain as a single composite action edge whose cost is the sum of each node cost in the chain and whose target is the final chain node's destination bounds. Cycles MUST be detected and rejected.
10. If a node lacks sufficient destination information (no `dest_*`/`dest_plane` and no resolvable chain providing it), it SHALL not produce an edge.

### Requirement 3: Cost model and heuristic
**User Story:** As a navigation client, I want realistic costs so paths prefer shorter travel while allowing teleports when beneficial.

#### Acceptance Criteria
1. All costs SHALL be expressed in milliseconds (ms).
2. Tile step cost SHALL be fixed at 600 ms per move for both cardinal and diagonal movement.
3. Door traversal additional cost SHALL be read from `door_nodes.cost` when present; if NULL or missing, default to 600 ms.
4. Lodestone teleport cost SHALL be read from `lodestone_nodes.cost` when present; if NULL or missing, default to 600 ms.
5. The heuristic SHALL be Chebyshev distance from current tile to goal multiplied by 600 ms (admissible and consistent for uniform 8-direction step costs).
6. Heuristic SHALL exclude door/lodestone additional costs to remain admissible.
7. Tie-breaking SHALL be deterministic (e.g., prefer lower g, then lower h, then lexicographic `(x,y,plane)`).
8. Action edge costs from `object_nodes`, `ifslot_nodes`, `npc_nodes`, and `item_nodes` SHALL use their respective `cost` fields; if NULL or missing, default to 600 ms.
9. For chained nodes via `next_node_type/next_node_id`, the composite edge cost SHALL be the sum of all individual node costs in the chain.

### Requirement 4: API and I/O
**User Story:** As a developer, I want a clear API I can call from services or scripts.

#### Acceptance Criteria
1. Provide a function `find_path(start, goal, options=None) -> PathResult` in Python, where:
   - `start`, `goal`: `(x:int, y:int, plane:int)`
   - `options` may override costs and set maximum expansions and timeouts
   - `PathResult` includes: `path: list[tuple[int,int,int]] | None`, `actions: list[ActionStep]`, `reason: str | None`, `expanded: int`, `cost_ms: int`
   - `ActionStep` schema:
     - `type`: `"move"|"door"|"lodestone"|"object"|"ifslot"|"npc"|"item"`
     - `from`: `(x:int,y:int,plane:int)`
     - `to`: `(x:int,y:int,plane:int)`
     - `cost_ms`: `int`
     - `node` (optional for non-move): `{ type: string, id: int }`
2. Provide a thin CLI entrypoint: `python -m navpath.astar --start "x,y,plane" --goal "x,y,plane" [--json]` printing the path or a clear message.
3. The module SHALL open the DB at `worldReachableTiles.db` by default with an option to pass a different path.
4. The `options` structure SHALL allow enabling/disabling action edges by node type (e.g., `use_doors`, `use_lodestones`, `use_objects`, `use_ifslots`, `use_npcs`, `use_items`) and SHALL default to all enabled.

### Requirement 5: Constraints, limits, and safety
**User Story:** As an operator, I need the service to be robust under large graphs and handle edge cases.

#### Acceptance Criteria
1. The search SHALL support early termination by:
   - `max_expansions` (e.g., default 250k)
   - `timeout_ms` (e.g., default 1000ms)
   - On termination, return `None` with `reason` ("max-expansions" or "timeout").
2. The implementation SHALL guard against missing or malformed DB rows (e.g., missing `allowed_directions`). Missing entries imply no moves from that tile.
3. The system SHALL log summary metrics at INFO level: expansions, frontier peak, path length, total cost, duration.
4. The system SHALL validate coordinates are integers within SQLite `tiles` domain and reject invalid input with a friendly message.
5. The system SHALL detect cycles in `next_node` chains and reject such nodes/edges. A configurable `max_chain_depth` (default 8) SHALL prevent pathological chains; exceeding it rejects the composite edge with reason "chain-depth-exceeded".
6. When origin/destination bounds are specified by nodes, the system SHALL interpret bounds inclusively; if bounds are invalid (min > max) the node is ignored.

### Requirement 6: Performance
**User Story:** As a user, I want responsive pathfinding for typical path lengths.

#### Acceptance Criteria
1. For intra-region routes (< 3000 steps), median runtime SHALL be <= 200ms on a modern CPU.
2. Memory footprint per search SHALL be O(V) with bounded priority queue growth.
3. The system SHALL avoid loading the entire DB into memory; use prepared statements/caching for neighbor lookups where appropriate.

## Non-Functional Requirements

### Code Architecture and Modularity
- Single Responsibility: DB access, graph expansion, heuristic/costs, and API/CLI separated into modules.
- Clear Interfaces: `GraphProvider` for neighbor and edge lookups; `AStarSearch` for algorithm; `CostModel` for weights.
- Dependency Management: Standard library + `sqlite3`; no heavy dependencies required.

### Security
- Read-only DB access; no writes to `worldReachableTiles.db`.
- Validate inputs; avoid SQL injection (use parameterized queries only).

### Reliability
- Deterministic outputs; unit tests for edge cases and typical paths.
- Fallback behaviors on missing data (skip edges, mark unreachable).

### Usability
- Path results serialize to JSON for CLI/API.
- Clear error reasons and metrics for observability.

## Open Questions
- Should lodestone teleport cost be different per destination or dynamic (e.g., based on travel time)? Default is a fixed cost (25.0).
- Should diagonal moves be permitted everywhere `allowed_directions` includes them, or require both adjacent cardinals unblocked? Default: trust `allowed_directions`.
- Any maximum path length or step cap beyond `max_expansions`?
