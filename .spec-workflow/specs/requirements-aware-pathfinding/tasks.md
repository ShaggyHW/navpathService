# Tasks: Requirements-aware Pathfinding

- [x] 1. Update DB models to expose requirement_id and add RequirementRow
  - File: navpath/db.py
  - Add `requirement_id: Optional[int]` to all node dataclasses: `DoorNodeRow`, `LodestoneNodeRow`, `ObjectNodeRow`, `IfslotNodeRow`, `NpcNodeRow`, `ItemNodeRow`.
  - Include `requirement_id` in all corresponding SELECT lists and constructors.
  - Add `RequirementRow` dataclass: `id:int, metaInfo:Optional[str], key:str, value:int, comparison:str`.
  - Add `fetch_requirement(req_id:int) -> Optional[RequirementRow]` with parameterized query.
  - Purpose: Provide the data needed for requirement gating with minimal changes to the rest of the system.
  - _Leverage: navpath/db.py existing patterns for fetch_* and iter_*; docs/tiles_nodes_schema.md requirements table_
  - _Requirements: R1, R2_
  - _Prompt: Role: Python Developer specializing in SQLite data access | Task: Modify navpath/db.py to expose requirement_id on all node rows and implement RequirementRow + fetch_requirement per the schema. Ensure all SELECT statements include requirement_id and typed constructors are updated. | Restrictions: Do not change write modes; keep read-only, parameterized SQL; preserve existing method names and signatures | _Leverage: navpath/db.py existing helpers; docs/tiles_nodes_schema.md | _Requirements: R1,R2 | Success: All node dataclasses have requirement_id; fetch_requirement works; code compiles; no behavior changes yet beyond added fields._

- [x] 2. Implement RequirementsEvaluator utility
  - File: navpath/requirements.py (new)
  - Implement `evaluate_requirement(req: RequirementRow, ctx_map: Dict[str,int]) -> bool` with operators `= != < <= > >=`, integer-only.
  - Purpose: Centralize deterministic requirement evaluation.
  - _Leverage: navpath/db.py RequirementRow_
  - _Requirements: R2_
  - _Prompt: Role: Python Engineer focused on utility libraries | Task: Create navpath/requirements.py with evaluate_requirement(req, ctx_map) implementing integer comparisons for =, !=, <, <=, >, >=. Missing key returns False. Unknown operator returns False. | Restrictions: Pure function; no I/O; no logging beyond optional type hints | _Leverage: RequirementRow definition_ | _Requirements: R2 | Success: Unit-level usage in graph/nodes produces expected booleans for all operators._

- [x] 3. Extend CLI to ingest requirements JSON and coerce booleans
  - File: navpath/__main__.py
  - Add flags: `--requirements-file PATH`, `--requirements-json JSON_STRING`.
  - Parse array of `{key,value}` objects; coerce `true/false` to `1/0`; validate `value` is int; merge last-wins per key.
  - Place final array into `SearchOptions.extras["requirements"]`.
  - Purpose: Provide runtime context for gating via CLI.
  - _Leverage: navpath/options.py SearchOptions; existing argparse patterns_
  - _Requirements: R3, R4_
  - _Prompt: Role: Python CLI Engineer | Task: Update navpath/__main__.py parser and _options_from_args to accept requirements JSON via file/string, coerce booleans to ints, validate structure, and populate options.extras["requirements"]. Add helpful error messages and update --help. | Restrictions: Keep backward compatibility; do not change required args; ensure non-zero exit on invalid JSON | _Leverage: argparse patterns in file_ | _Requirements: R3,R4 | Success: CLI accepts inputs, errors clearly on invalid, and passes array into extras._

- [x] 4. Normalize requirements in API and add logging metric
  - File: navpath/api.py
  - On entry, if `options.extras["requirements"]` is present, build `requirements_map: Dict[str,int]` and store at `options.extras["requirements_map"]` for fast lookups.
  - After search returns, include `req_filtered` from graph provider (default 0) in the INFO summary line.
  - Purpose: Efficient lookups and visibility into gating.
  - _Leverage: navpath/api.py existing logging; SearchOptions.extras_
  - _Requirements: R3, R5_
  - _Prompt: Role: Backend Python Developer | Task: Update navpath/api.py to normalize requirements list to a map in extras and log req_filtered in the summary info (fallback 0 if attribute missing). | Restrictions: Do not change function signature; preserve existing metrics | _Leverage: existing logging format in find_path()_ | _Requirements: R3,R5 | Success: Normalization occurs without side effects; logs include req_filtered._

- [x] 5. Gate neighbors in SqliteGraphProvider
  - File: navpath/graph.py
  - Build `ctx_map` from `options.extras["requirements_map"]` if present, else from `options.extras["requirements"]`.
  - Maintain `self.req_filtered_count: int`.
  - For every candidate neighbor derived from a node row, if `requirement_id` is not None: fetch requirement (with small in-memory cache by id) and evaluate; skip and increment counter if unmet.
  - Apply gating to doors, lodestones, and action edges prior to deterministic ordering.
  - Purpose: Enforce gating uniformly and early.
  - _Leverage: navpath/db.py fetch_requirement; navpath/requirements.py evaluate_requirement; existing neighbor code_
  - _Requirements: R1, R5, R6_
  - _Prompt: Role: Graph/A* Engineer | Task: Integrate requirement gating into navpath/graph.py to filter nodes before emitting edges, counting req_filtered, preserving deterministic order. Implement a small LRU/dict cache for requirement rows. | Restrictions: No behavioral changes other than gating; do not alter cost/heuristic; keep sorting stable | _Leverage: existing graph provider patterns_ | _Requirements: R1,R5,R6 | Success: Edges from unmet requirements are never yielded; req_filtered_count increments properly; performance remains acceptable._

- [x] 6. Gate next_node chains in NodeChainResolver
  - File: navpath/nodes.py
  - Before processing each chain link, apply the same gating. If any link fails, abort chain resolution for that head.
  - Purpose: Ensure action chains respect requirements across all links.
  - _Leverage: navpath/requirements.py; navpath/db.py fetch_requirement; existing resolver hooks_
  - _Requirements: R1, R2_
  - _Prompt: Role: Python Engineer experienced with graph action chains | Task: Update NodeChainResolver to evaluate and enforce requirement gating per link using the same evaluator and context map; abort chain on any unmet requirement. | Restrictions: Preserve cycle detection and bounds logic; do not change cost aggregation | _Leverage: existing chain resolution code_ | _Requirements: R1,R2 | Success: Chains with unmet requirements are excluded deterministically._

- [x] 7. Documentation updates
  - File: navpath/README.md
  - Document the new CLI flags, JSON array shape, boolean coercion behavior, and API usage via `SearchOptions.extras`.
  - Purpose: Ensure users can configure gating correctly.
  - _Leverage: navpath/README.md style and sections_
  - _Requirements: R3, R4_
  - _Prompt: Role: Technical Writer | Task: Update navpath/README.md to include requirements-aware configuration examples for CLI and API, including sample JSON and logging metrics. | Restrictions: Keep tone and structure consistent; ensure examples are copy-pasteable | _Leverage: existing README structure_ | _Requirements: R3,R4 | Success: README clearly explains usage; examples work._

- [ ] 8. Wire req_filtered to API logs
  - File: navpath/graph.py, navpath/api.py
  - Expose a method or property on the graph provider to retrieve `req_filtered_count` for logging.
  - Ensure `find_path()` logs it as part of the metrics line.
  - Purpose: Visibility into gating.
  - _Leverage: existing logging in api.py_
  - _Requirements: R5_
  - _Prompt: Role: Backend Developer | Task: Add a simple accessor for req_filtered_count and include it in api logging. | Restrictions: Maintain backward compatibility | _Leverage: api.py logger_ | _Requirements: R5 | Success: Logs include req_filtered consistently._

- [ ] 9. Optional: Basic integration test harness (smoke)
  - File: scripts/smoke_requirements.json (example), and a short test script or documented manual command
  - Provide a small example JSON and CLI command that demonstrates gating behavior (e.g., a node with requirement key not met is skipped).
  - Purpose: Aid manual verification until a formal test suite exists.
  - _Leverage: CLI and README_
  - _Requirements: R1, R4_
  - _Prompt: Role: Developer Advocate | Task: Add a simple example JSON and command snippet to verify gating locally. | Restrictions: Do not introduce heavy test dependencies | _Leverage: existing CLI_ | _Requirements: R1,R4 | Success: Users can reproduce gating locally following the example._

## Notes for Implementers
- Before starting a task, edit this file and change `[ ]` to `[-]` for the task you're working on. When done, change `[-]` to `[x]`.
- Use small, focused commits mapped to these tasks.
