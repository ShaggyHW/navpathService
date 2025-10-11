# Tasks: navmesh-astar

- [x] 1. Create NavmeshDatabase (read-only helpers for navmesh.db)
  - File: navpath/navmesh_db.py
  - Define typed rows for `RegionRow` and `RegionEdgeRow` and connection helpers (read-only, parameterized). Implement:
    - `connect(path) -> NavmeshDatabase`
    - `has_navmesh() -> bool` (detects presence of `nav_regions`)
    - `fetch_region_by_tile(x,y,plane) -> Optional[RegionRow]`
    - `iter_region_edges(src_region_id) -> Iterator[RegionEdgeRow]`
    - `iter_region_tiles(region_id) -> Iterator[Tile]`
    - `tile_exists(tile: Tile) -> bool`
    - `fetch_metadata_key(key: str) -> Optional[str]`
  - Purpose: Provide a safe, typed access layer to `navmesh.db` tables (`nav_regions`, `nav_region_edges`, `region_tiles`, `metadata`).
  - _Leverage: `navpath/db.py` (connection pattern, typed rows), `docs/navmesh_schema.md`_
  - _Requirements: R9_
  - _Prompt: Implement the task for spec navmesh-astar, first run spec-workflow-guide to get the workflow guide then implement the task: Role: Python Developer specialized in SQLite access layers | Task: Create `NavmeshDatabase` with read-only connection and typed queries for navmesh tables per R9; mirror style from `navpath/db.py` while targeting `navmesh.db` schema | Restrictions: Read-only queries only (SELECT), parameterized SQL, deterministic ordering where applicable, do not change existing `Database` class | _Leverage: `navpath/db.py`, `docs/navmesh_schema.md` | _Requirements: R9 | Success: Module compiles with typed dataclasses, queries return expected shapes against a valid `navmesh.db`, and `has_navmesh()` correctly detects presence of `nav_regions`. Mark this task in `.spec-workflow/specs/navmesh-astar/tasks.md` as in-progress `[-]` when starting and `[x]` when done._

- [x] 2. Implement NavmeshGraphProvider (GraphProvider over regions)
  - File: navpath/navmesh_graph.py
  - Create `NavmeshGraphProvider` implementing `neighbors(tile, goal, options)` with deterministic ordering and parity with `SqliteGraphProvider` semantics. Include:
    - Resolve `src_region` from `region_tiles`. If missing, raise `TileNotFoundError` (parity with `graph.py`).
    - Movement edges from `nav_region_edges(type='move')`: use `meta.border_sample` when present; fallback to deterministic shared-boundary scan; cost via `CostModel.movement_cost`.
    - Special edges (`door|lodestone|object|ifslot|npc|item`): requirement gating via `meta.requirement_id` (treat missing requirement rows as unmet), apply per-type cost overrides via `CostModel`, attach `NodeRef` when applicable (e.g., `meta.head_id`), and copy `meta` into `Edge.metadata`. If `meta.chain` exists, embed in `Edge.metadata['chain']` for `_reconstruct()`.
    - Lodestones only from `options.extras['start_tile']` to match branching reduction strategy.
    - Deterministic sorting comparable to `SqliteGraphProvider` (`navpath/graph.py`).
    - Track `req_filtered_count` like the tile provider.
  - Purpose: Produce tile-to-tile edges from region graph preserving output and action semantics.
  - _Leverage: `navpath/graph.py` (Edge, GraphProvider, ordering, gating), `navpath/cost.py`, `navpath/requirements.py`, `docs/navmesh_schema.md`_
  - _Requirements: R3, R5, R6, R7_
  - _Prompt: Implement the task for spec navmesh-astar, first run spec-workflow-guide to get the workflow guide then implement the task: Role: Senior Python Engineer with expertise in graph search | Task: Implement `NavmeshGraphProvider` yielding deterministic neighbors from `navmesh.db` per design; ensure requirement gating, chain propagation, and cost overrides mirror existing provider behavior | Restrictions: Do not modify `astar.py` or `path.py`; keep outputs identical in shape; avoid nondeterministic iteration | _Leverage: `navpath/graph.py`, `navpath/cost.py`, `navpath/requirements.py`, `docs/navmesh_schema.md` | _Requirements: R3, R5, R6, R7 | Success: Deterministic neighbor ordering, correct gating and costs, and `_reconstruct()` expands chains from `Edge.metadata['chain']`. Update this task status markers accordingly._

- [x] 3. Provider selection in API (auto-detect DB type)
  - File: navpath/api.py (modify existing)
  - Detect if the connected DB has `nav_regions`; if so, instantiate `NavmeshDatabase + NavmeshGraphProvider`; otherwise, use existing `Database + SqliteGraphProvider`. Keep function signature and logging unchanged. Ensure start/goal validation and `tile-not-found` reasoning align with the selected provider's existence checks.
  - Purpose: Maintain API parity while supporting navmesh transparently based on DB contents.
  - _Leverage: `navpath/api.py`, `navpath/db.py`, `navpath/graph.py`, new `navpath/navmesh_db.py`, `navpath/navmesh_graph.py`_
  - _Requirements: R1, R2, R8, R10_
  - _Prompt: Implement the task for spec navmesh-astar, first run spec-workflow-guide to get the workflow guide then implement the task: Role: Python Backend Engineer focusing on public API stability | Task: Update `find_path()` to detect DB type and choose the appropriate provider while preserving metrics logging and return shapes | Restrictions: Do not change function signature or CLI flags; maintain metrics format; no breaking changes | _Leverage: `navpath/api.py`, `navpath/db.py`, `navpath/graph.py`, `navpath/path.py` | _Requirements: R1, R2, R8, R10 | Success: Both tile DB and navmesh DB work with identical API/CLI behavior; metrics line unchanged; detection is reliable._

- [x] 4. Deterministic tile selection utility for regions/borders
  - File: navpath/navmesh_graph.py (internal helper) or navpath/mesh_tiles.py
  - Implement deterministic selection: prefer `border_sample` when present; else scan by `x` then `y` within region/border; ensure `tile_exists()`; use plane from region.
  - Purpose: Ensure reproducible tile-level edges from region semantics.
  - _Leverage: `navpath/graph.py` `_select_dest_tile()` pattern, `docs/navmesh_schema.md`_
  - _Requirements: R5, R3_
  - _Prompt: Implement the task for spec navmesh-astar, first run spec-workflow-guide to get the workflow guide then implement the task: Role: Algorithmic Engineer | Task: Implement deterministic tile selection helpers used by `NavmeshGraphProvider` | Restrictions: Must be deterministic across runs; no randomization; avoid DB full scans | _Leverage: `navpath/graph.py` patterns | _Requirements: R5, R3 | Success: Helper returns the same tile consistently for the same inputs; covered by unit tests._

- [ ] 5. Unit tests: Navmesh DB and Graph Provider
  - Files: tests/test_navmesh_db.py, tests/test_navmesh_graph.py
  - Add tests for: `has_navmesh()`, `fetch_region_by_tile()`, movement edge generation, special edge gating and cost overrides, and deterministic ordering. Use a small fixture `navmesh.db` or ephemeral test DB created via schema SQL.
  - Purpose: Validate correctness and determinism of the new components.
  - _Leverage: `docs/navmesh_schema.md`, `navpath/graph.py` tests patterns (if any)_
  - _Requirements: R3, R6, R7, R9_
  - _Prompt: Implement the task for spec navmesh-astar, first run spec-workflow-guide to get the workflow guide then implement the task: Role: QA Engineer (Python/pytest) | Task: Create unit tests covering DB helpers and provider behavior; include unmet requirement gating and override scenarios | Restrictions: Tests must be deterministic; isolate from external state; skip gracefully if fixture DB absent | _Leverage: `docs/navmesh_schema.md` | _Requirements: R3, R6, R7, R9 | Success: Tests pass locally; clear coverage of ordering, gating, and costs._

- [ ] 6. Integration: API provider selection and CLI parity
  - Files: tests/test_api_navmesh_detection.py
  - Add tests that call `find_path()` with a navmesh DB to verify provider selection, metrics log line content, return type and JSON shape parity; run CLI with `--json` and `--json-actions-only` to validate outputs.
  - Purpose: Ensure drop-in compatibility at API and CLI layers.
  - _Leverage: `navpath/__main__.py`, `navpath/api.py`, `navpath/path.py`_
  - _Requirements: R1, R2, R8, R10_
  - _Prompt: Implement the task for spec navmesh-astar, first run spec-workflow-guide to get the workflow guide then implement the task: Role: SDET | Task: Write integration tests for provider selection and CLI output parity | Restrictions: Do not change CLI flags; assert JSON keys and types only; tolerate path variability | _Leverage: `navpath/__main__.py`, `navpath/api.py` | _Requirements: R1, R2, R8, R10 | Success: Tests pass and confirm identical shapes and metrics across DB types._

- [x] 7. Documentation updates
  - Files: navpath/README.md (update), docs/navmesh_schema.md (verify references)
  - Update README to mention navmesh support and provider auto-detection; keep API/CLI usage examples unchanged. Ensure schema doc cross-references are correct.
  - Purpose: Communicate capabilities without changing UX.
  - _Leverage: `navpath/README.md`, `docs/navmesh_schema.md`_
  - _Requirements: R1, R2, R10_
  - _Prompt: Implement the task for spec navmesh-astar, first run spec-workflow-guide to get the workflow guide then implement the task: Role: Technical Writer | Task: Update docs to reflect navmesh-backed search with the same API/CLI; add brief provider-detection note | Restrictions: Do not alter existing examples' flags/outputs; keep concise | _Leverage: README and docs | _Requirements: R1, R2, R10 | Success: Docs clearly state navmesh support; examples still valid._

- [ ] 8. Performance sanity checks and logging
  - Files: optional benchmark script under scripts/bench_navmesh.py
  - Add a simple script to compare expansions and durations between tile DB and navmesh DB on representative routes; ensure INFO metrics log line remains identical in key order.
  - Purpose: Validate performance and logging parity.
  - _Leverage: `navpath/api.py` metrics, `navpath/astar.py`_
  - _Requirements: R3, R8, Performance NFR_
  - _Prompt: Implement the task for spec navmesh-astar, first run spec-workflow-guide to get the workflow guide then implement the task: Role: Performance Engineer | Task: Create a quick benchmark script and confirm logging parity | Restrictions: Read-only; avoid external dependencies | _Leverage: `navpath/api.py` | _Requirements: R3, R8 | Success: Script runs and prints comparable metrics with expected key order._
