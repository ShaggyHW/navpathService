# Requirements Document

## Introduction

A Python script and accompanying output schema to build a static navmesh database from the existing `worldReachableTiles.db`. The tool reads tile connectivity and special transition nodes (doors, lodestones, objects, NPCs, items), then generates a compact, indexed SQLite navmesh optimized for fast runtime pathfinding.

Value: precomputing a navmesh shifts work from runtime to build-time, improving responsiveness and enabling consistent, reproducible pathfinding across planes.

## Alignment with Product Vision

This feature provides a deterministic, reproducible navigation data source for `navpath` to consume. It reduces runtime computation and centralizes the logic for interpreting tiles and special traversal mechanics into a single build artifact.

## Requirements

### Requirement 1: Build navmesh region graph from tiles

**User Story:** As a developer/operator, I want a CLI to generate a navmesh DB from `worldReachableTiles.db` using grouped walk regions so that runtime pathfinding is fast and consistent.

#### Acceptance Criteria

1. WHEN the CLI is executed with `--input <path/to/worldReachableTiles.db>` and `--output <path/to/navmesh.db>` THEN the system SHALL create a new SQLite database at the output path.
2. WHEN reading `tiles(x,y,plane,allowed_directions,blocked_directions,category,tiledata)` THEN the system SHALL compute walk regions by grouping contiguous walkable tiles per plane using a deterministic flood-fill/union-find. `blocked_directions` SHALL be treated as boundaries. Regions SHALL not be merged across diagonal-only contacts (must share a side transition) and SHALL not merge across door boundaries by default.
3. The system SHALL produce a region graph where each region is a node. Intra-region movements are collapsed; adjacency edges SHALL only exist between neighboring regions that share at least one valid tile-to-tile transition on the border.
4. The system SHALL preprocess node tables to generate region connectors: doors connect interior/exterior regions; lodestones connect from any region to the destination region (subject to requirements and design in Requirement 2); object/NPC/ifslot/item nodes map origin/destination bounds deterministically to source/destination regions, preserving original node identifiers in metadata.
5. The system SHALL persist a mapping from tiles to their region for traceability and coordinate localization.
6. The system SHALL support multiple planes and generate cross-plane edges only when defined by special transitions (see Requirement 2).
7. The output DB SHALL include indexes to support fast lookups by region id and coordinate-to-region mapping.

### Requirement 2: Integrate special transitions from node tables

**User Story:** As a pathfinding engineer, I want special transitions (doors, teleports, object/NPC interactions, UI slots, items) represented as weighted edges so that routes can traverse interactable mechanics when allowed.

#### Acceptance Criteria

1. GIVEN `door_nodes` THEN the system SHALL add directed edges between interior/exterior tiles based on `direction` and locations, honoring `requirement_id` and applying `cost` when present.
2. GIVEN `lodestone_nodes` THEN the system SHALL add teleport edges from any coordinate within the lodestone’s origin semantics (by design) to `dest_x,dest_y,dest_plane`, with optional `cost` and requirement gating.
3. GIVEN `object_nodes` and `npc_nodes` THEN the system SHALL add edges into destination bounds (`dest_min/max_x/y`, `dest_plane`) from origin bounds (if specified) honoring `match_type`, `action`, requirement, and `search_radius` semantics.
4. GIVEN `ifslot_nodes` and `item_nodes` THEN the system SHALL add edges that represent UI and item-triggered transitions into specified destination bounds and planes with optional `cost` and requirement gating.
5. WHEN `requirement_id` references `requirements(id)` THEN the system SHALL encode gating metadata and exclude such edges unless requirements are considered satisfied at evaluation time (persist condition in edge metadata; evaluation policy is runtime concern).
6. All special transition edges SHALL be typed (e.g., `door|lodestone|object|npc|ifslot|item`) and preserve source node identifiers in metadata for traceability.

### Requirement 3: Cost model

**User Story:** As a developer, I want consistent traversal costs so that pathfinding prefers efficient routes and uses specials when beneficial.

#### Acceptance Criteria

1. The system SHALL assign base grid-move cost (default 1) to tile adjacency edges.
2. The system SHALL assign special edges their `cost` if specified; otherwise default to a configured special-edge base (default 1 unless otherwise configured per type).
3. The CLI SHALL support optional cost multipliers: `--cost-base <n>`, `--cost-door <n>`, `--cost-lodestone <n>`, etc.

### Requirement 4: Determinism and idempotence

**User Story:** As an operator, I want reproducible outputs so that builds are verifiable and cacheable.

#### Acceptance Criteria

1. GIVEN the same input DB and config, THEN the output `navmesh.db` SHALL be byte-for-byte identical (aside from timestamps) across runs.
2. The output DB SHALL record provenance in `metadata` (input path hash, source schema snapshot, build timestamp, tool version, config hash).

### Requirement 5: Validation, reporting, and safety

**User Story:** As a maintainer, I want clear logs and summary stats to validate the build.

#### Acceptance Criteria

1. The CLI SHALL print a build summary: cells count, edges count (grid/special by type), planes, and counts per transition type.
2. The system SHALL warn on orphan specials (e.g., destination outside any `tiles` record) and skip invalid edges.
3. The build SHALL fail with nonzero exit code for unrecoverable errors (input missing, DB locked, schema mismatch).

### Requirement 6: CLI and configuration

**User Story:** As a user, I need a straightforward interface to run the build locally or in CI.

#### Acceptance Criteria

1. CLI name: `python -m navpath.navmesh_build` (or `navmesh_build.py` entrypoint) with options:
   - `--input PATH` (required)
   - `--output PATH` (required)
   - `--planes <list|all>` filter (optional)
   - `--dry-run` (compute stats only; no DB write)
   - `--overwrite` (allow replacing existing output)
   - cost flags from Requirement 3
2. Help text (`-h/--help`) SHALL describe all options and defaults.

### Requirement 7: Output schema

**User Story:** As a consumer service, I want a compact schema to query neighbors and costs efficiently.

#### Acceptance Criteria

1. The output SQLite DB SHALL include tables:
   - `nav_regions(id INTEGER PRIMARY KEY, plane INT, min_x INT, min_y INT, max_x INT, max_y INT, area INT, category TEXT, meta TEXT)` where `meta` is JSON (e.g., region generation hints).
   - `nav_region_edges(src_region_id INT, dst_region_id INT, weight REAL, type TEXT, meta TEXT)` where `meta` is JSON (original node ids, requirements, action, chain metadata, etc.).
   - `region_tiles(region_id INT, x INT, y INT, plane INT)` mapping tiles to their parent region for traceability.
   - `metadata(key TEXT PRIMARY KEY, value TEXT)`
2. Indexes SHALL be created:
   - `CREATE INDEX idx_regions_plane ON nav_regions(plane);`
   - `CREATE INDEX idx_region_tiles_xyz ON region_tiles(x,y,plane);`
   - `CREATE INDEX idx_region_edges_src ON nav_region_edges(src_region_id);`
   - `CREATE INDEX idx_region_edges_dst ON nav_region_edges(dst_region_id);`
3. Foreign keys optional for performance; integrity ensured by build process.

## Non-Functional Requirements

### Code Architecture and Modularity
- Single Responsibility: separate modules for input schema reading, graph construction, special extractors, and DB writing.
- Modular Design: pluggable extractor per node table (`door`, `lodestone`, `object`, `npc`, `ifslot`, `item`).
- Clear Interfaces: define `CellBuilder`, `EdgeBuilder`, and per-extractor interfaces.

### Performance
- Streamed processing by plane/region to limit memory usage.
- Batch inserts and `PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;` when safe.
- Avoid N+1 queries; use prepared statements.

### Security
- Only local file IO; no network access.
- Validate input paths; do not execute arbitrary SQL from the input.

### Reliability
- Use transactions with chunked commits; rollback on fatal errors.
- Deterministic iteration order (sorted coordinates, stable ids).

### Usability
- Progress output per phase; clear warnings and actionable errors.
- Configuration via CLI flags and optional `toml`/`yaml` config file.
