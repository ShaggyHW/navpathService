# Requirements: navmesh-astar

## Introduction
Implement A* pathfinding over the region-based navigation mesh stored in `navmesh.db`, providing the same public API, CLI options, output formats, and behavior as the existing tile/node-based `navpath` implementation. The new implementation must remain a drop-in replacement from a caller’s perspective.

## Alignment with Product Vision
- Reuse the deterministic A* patterns, options, and result models from `navpath` to ensure consistent UX across data backends.
- Add support for region-graph navigation (`nav_regions`, `nav_region_edges`) to leverage pre-aggregated connectivity for performance and scalability.

## Requirements

### R1: Public API parity
**User Story:** As an integrator, I want the same `find_path()` API so that I can switch to navmesh without code changes.

- The function signature remains `find_path(start, goal, options=None, db_path=None) -> PathResult` (see `navpath/api.py`).
- `start`, `goal` are tiles `(x,y,plane)` with the same validation rules as `navpath`.
- `db_path` points to a `navmesh.db` by default; an explicit path or `options.extras['db_path']` overrides it.
- Returns the same `PathResult` object shape with identical fields and semantics.

#### Acceptance Criteria
1. WHEN calling `find_path((x1,y1,p),(x2,y2,p))` with a valid `navmesh.db` THEN the return type is `PathResult` with identical JSON shape to `navpath`.
2. IF inputs are not `(int,int,int)` tuples THEN reason SHALL be `"invalid-input"`.
3. IF either tile is not present in `region_tiles` THEN reason SHALL be `"tile-not-found"`.

### R2: CLI parity
**User Story:** As a CLI user, I want identical flags and outputs so that existing scripts continue to work.

- The CLI in `navpath/__main__.py` remains unchanged (same flags and help text).
- All toggles, limits, cost overrides, logging, and requirements inputs behave the same.
- Human-readable and JSON outputs are byte-for-byte compatible in structure and key names.

#### Acceptance Criteria
1. WHEN running `python -m navpath --start "x,y,p" --goal "x,y,p" --json` THEN JSON keys and types match the current implementation.
2. WHEN using `--json-actions-only` THEN only the `actions` array is emitted with the same per-step shape.
3. WHEN `--requirements-file` or `--requirements-json` are supplied THEN requirement gating applies identically.

### R3: Deterministic A* behavior
**User Story:** As a developer, I want deterministic results so that tests and automation are stable.

- Stable priority queue ordering, with the same tie-breakers as `navpath/astar.py`.
- Deterministic neighbor ordering from the region graph provider.

#### Acceptance Criteria
1. GIVEN fixed DB and options WHEN run multiple times THEN the same path and action list are returned.
2. WHEN multiple edges have equal `f,h,g` THEN the same tie-breaking order is observed.

### R4: Limits and cancellation
**User Story:** As an operator, I want to limit search work to prevent overload.

- Respect `options.max_expansions` and `options.timeout_ms` with the same stop reasons.

#### Acceptance Criteria
1. IF `max_expansions` exceeded THEN reason SHALL be `"max-expansions"`.
2. IF `timeout_ms` exceeded THEN reason SHALL be `"timeout"`.

### R5: Output parity (tiles and actions)
**User Story:** As a consumer, I want identical path and action output formats so my downstream logic keeps working.

- `PathResult.path` is a list of tile tuples `(x,y,plane)` or `None`.
- `PathResult.actions` is a list of `ActionStep` with the same fields and chain semantics.
- Region-to-tile reconstruction uses `region_tiles` to choose concrete tiles deterministically (e.g., along edge border samples or ordered scan by x then y within target region when needed), preserving existing rules for action chain emission.

#### Acceptance Criteria
1. WHEN a path exists THEN `path` contains tiles and `actions` expand chains with the same per-link metadata keys used by `navpath`.
2. WHEN no path exists THEN `path` is `null` and reason is one of: `"unreachable"|"timeout"|"max-expansions"|"tile-not-found"|"invalid-input"`.

### R6: Requirements-aware gating
**User Story:** As a caller, I want requirement-based availability to work the same way.

- Accept requirements via CLI or `SearchOptions.extras["requirements"]` (list of `{key,value}`), with normalization to `extras["requirements_map"]`.
- Evaluate requirement rows referenced by region edges (`meta.requirement_id`) using the same integer comparison semantics as in `worldReachableTiles.db`.

#### Acceptance Criteria
1. GIVEN unmet requirement for an edge THEN that edge SHALL be suppressed from expansion.
2. GIVEN a mix of unmet and met edges THEN only met edges are considered.

### R7: Cost model and heuristic
**User Story:** As a tuner, I want consistent cost semantics and overrides.

- Default movement step and node/action costs match `navmesh.db` weight semantics while honoring the same override flags (`--door-cost`, `--lodestone-cost`, etc.).
- Heuristic remains Chebyshev × step cost on tiles; region-level edges must convert to tile-level distances for admissibility or use per-edge weights that guarantee optimality when reconstructed to tiles.

#### Acceptance Criteria
1. WHEN no overrides are provided THEN reported `total_cost_ms` equals the sum of chosen edges’ weights.
2. WHEN overrides are provided THEN they apply consistently per type.

### R8: Logging and metrics parity
**User Story:** As a maintainer, I want the same concise metrics for observability.

- INFO log includes: `start`, `goal`, `reason`, `expanded`, `path_len`, `total_cost_ms`, `duration_ms`, `req_filtered`, `db` (same keys and order as `navpath/api.py`).

#### Acceptance Criteria
1. WHEN a search completes THEN a single INFO log line is emitted with the same format.

### R9: Database access and safety
**User Story:** As a DBA, I want the system to be read-only and resilient.

- All SQLite access is read-only; only `SELECT` queries are executed.
- The schema consumed is: `nav_regions`, `nav_region_edges`, `region_tiles`, `metadata`.
- No assumptions about FKs; code must not rely on FK enforcement.

#### Acceptance Criteria
1. WHEN connected to a valid `navmesh.db` THEN queries succeed with read-only pragmas and no writes.
2. IF tables are missing or malformed THEN fail gracefully with a clear reason or exception surfaced to the caller.

### R10: Backward compatibility and migration
**User Story:** As a user, I want to adopt navmesh without breaking integrations.

- The `navpath` package keeps its module layout and CLI; switching DBs changes behavior only in pathfinding back-end.
- Feature flags and defaults retain their current names and defaults.

#### Acceptance Criteria
1. WHEN switching between `worldReachableTiles.db` and `navmesh.db` by changing `db_path` THEN the interface and outputs remain consistent.

## Non-Functional Requirements

### Code Architecture and Modularity
- **Single Responsibility Principle:** New provider(s) isolate navmesh concerns from existing tile+nodes provider.
- **Modular Design:** Introduce `NavmeshGraphProvider` and supporting DB layer without altering `astar` or result models.
- **Clear Interfaces:** Reuse `GraphProvider` and `CostModel` contracts; keep `SearchOptions` unchanged.

### Performance
- Region graph must reduce expansions vs. tile graph on large areas. Target at least 2× fewer expansions on representative routes.
- End-to-end latency target: within ±10% of current `navpath` for comparable paths (or faster).

### Security
- Read-only DB connections; sanitize inputs; no dynamic SQL from user-provided text.

### Reliability
- Deterministic behavior; defensive handling for missing region tiles; robust chain reconstruction.

### Usability
- CLI and logging remain unchanged; errors and reasons align with existing values.
