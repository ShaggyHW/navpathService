# Requirements: Requirements-aware Pathfinding

## Introduction

Add requirements-aware gating to pathfinding so that nodes in the SQLite world graph are only considered if their referenced requirement is satisfied by caller-supplied data. Each node table may reference `requirements(id)` via a `requirement_id` column. When a node has a `requirement_id`, the search must consult a per-request "requirements context" and ignore the node if the requirement is not met or if there is no data provided for that requirement key.

Canonical requirements context format is a JSON array of dynamic key/value pairs, where values are integers (booleans accepted by CLI and coerced to 0/1):

```json
[
  { "key": "hasKey", "value": 1 },
  { "key": "questStage", "value": 23 },
  { "key": "spellUnlocked", "value": 0 }
]
```

The comparison operator is stored in the DB in `requirements.comparison` and is used during evaluation. This array format is flexible for an ever-expanding set of requirement keys.

Value:
- Ensures paths do not use actions/teleports/doors that are unavailable to the player/client context.
- Provides deterministic, explicit filtering consistent with A*’s optimality and stable ordering.

## Alignment with Product Vision

- Reliability and correctness: Do not produce infeasible paths.
- Extensibility: Dynamic JSON format allows new requirement keys without changing the input shape.
- Determinism: Filtering happens before neighbor ordering, preserving stable expansion order.

## Background and Current State

- Public API `navpath.api.find_path(start, goal, options=None, db_path=None)` orchestrates DB, graph, and A*.
- CLI `python -m navpath` supports toggles, limits, and outputs, but currently does not ingest any requirement context.
- Schema (`docs/tiles_nodes_schema.md`) lists node tables with `requirement_id` referencing `requirements(id)`. The `requirements` table has columns: `id`, `metaInfo` (TEXT), `key` (TEXT), `value` (INTEGER), `comparison` (TEXT).
- Node tables: `door_nodes`, `lodestone_nodes`, `object_nodes`, `ifslot_nodes`, `npc_nodes`, `item_nodes`.

## Requirements

### R1. Node gating based on requirement context

- IF a node row has a non-NULL `requirement_id`, THEN the node SHALL be considered eligible only if the requirement evaluates to true against the caller-supplied requirement context.
- IF there is no entry for the requirement `key` in the provided context JSON array, THEN the requirement SHALL be treated as not available and the node SHALL be ignored.
- Applies to all node sources, including chain links resolved via `next_node_type`/`next_node_id`. A single unmet requirement anywhere in the chain SHALL cause the entire action chain to be excluded.

Acceptance Criteria
1. WHEN pathfinding runs with a requirement context that satisfies a node’s requirement, THEN that node MAY be used in the returned path (subject to A* search).
2. WHEN a node has a `requirement_id` and the requirement is not satisfied, THEN that node SHALL not appear in any explored neighbor set and SHALL not contribute to actions.
3. WHEN a node has a `requirement_id` and the requirement `key` is absent from the JSON array, THEN that node SHALL be ignored.

### R2. Requirement evaluation semantics

- All evaluations are integer-based: the provided context `value` (integer) is compared to the requirement row’s `value` (INTEGER) using the `comparison` operator from the DB.
- Supported operators (limited to): `=`, `!=`, `<`, `<=`, `>`, `>=`.
- Type handling and validation:
  - CLI SHALL accept boolean literals `true`/`false` and coerce them to `1`/`0` respectively before validation.
  - After coercion, the context `value` MUST be an integer.
  - Non-integer `value` inputs (after coercion) SHALL be rejected at CLI parse-time with a clear error. API callers SHOULD validate before passing.

Acceptance Criteria
1. GIVEN row `(key="hasKey", value=1, comparison="=")` and context `[{"key":"hasKey","value":1}]`, THEN evaluation is true; `[{"key":"hasKey","value":0}]` is false; `[]` is false.
2. GIVEN row `(key="questStage", value=30, comparison=">=")` and context `[{"key":"questStage","value":35}]`, THEN evaluation is true; `[{"key":"questStage","value":29}]` is false; `[]` is false.

### R3. API support for requirement context

- `find_path()` SHALL accept requirement context via `SearchOptions.extras["requirements"]` as a list of `{ "key": str, "value": int }` pairs.
- If `extras["requirements"]` is missing or not a list, behavior SHALL be as if the list is empty.
- No signature change to `find_path()` required; API remains backward compatible.

Acceptance Criteria
1. WHEN `find_path()` is called with `options.extras = {"requirements": [{"key":"hasKey","value":1}]}`, THEN the graph layer SHALL gate nodes according to that list.
2. WHEN `find_path()` is called without any `requirements` extras, THEN nodes with non-NULL `requirement_id` SHALL be ignored.

### R4. CLI ingestion of requirement context JSON

- The CLI SHALL accept requirement context via one of the following inputs:
  - `--requirements-file PATH` pointing to a JSON file containing an array of `{key,value}` objects.
  - `--requirements-json JSON_STRING` directly on the command line containing the same array shape.
- JSON shape (canonical):

```json
[
  { "key": "hasKey", "value": 1 },
  { "key": "questStage", "value": 23 },
  { "key": "spellUnlocked", "value": 0 }
]
```

- Boolean convenience: CLI SHALL coerce `true`/`false` to `1`/`0` prior to validation.
- If both flags are provided, `--requirements-json` SHALL override pairs from `--requirements-file` on a per-key last-wins basis.
- The parsed array SHALL be placed into `SearchOptions.extras["requirements"]` for consumption by the graph.
- Invalid JSON or non-integer `value` (after coercion) SHALL produce a clear CLI parse error and exit with non-zero status.

Acceptance Criteria
1. WHEN `--requirements-file` is provided and readable, THEN its JSON array is loaded and used for gating.
2. WHEN `--requirements-json` is provided, THEN it is parsed and used (overriding any overlapping keys from file).
3. WHEN neither is provided, THEN the effective list is empty and nodes with requirements are ignored.
4. Invalid JSON or non-integer values (after coercion) result in a non-zero exit and a clear error message.

### R5. Logging and metrics

- `find_path()` summary logging SHALL include a count of nodes/edges filtered due to unmet requirements (aggregate count), exposed in the `INFO` summary line or as a separate DEBUG detail.
- No change to output JSON shape for `PathResult` is required in this phase.

Acceptance Criteria
1. WHEN gating excludes nodes, THEN a counter is emitted in logs (e.g., `req_filtered=123`).

### R6. Determinism and performance

- Determinism SHALL be preserved: the presence of gating must not introduce nondeterministic ordering.
- Performance impact SHALL be minimal; evaluation SHOULD occur as early as practical (preferably within SQL WHERE clauses or immediately after row fetch, before neighbor sorting).

Acceptance Criteria
1. Benchmark on representative queries shows no more than a small constant-factor overhead under gating (implementation to validate in design).

## Non-Functional Requirements

### Code Architecture and Modularity
- Introduce a focused requirement evaluation utility (e.g., `RequirementsEvaluator`) to keep graph logic clean.
- Centralize requirement parsing and operator handling.
- Integrate gating into both simple node expansions and chain resolution.

### Security
- CLI must treat requirement inputs as untrusted JSON; do not execute code. Validate types defensively.

### Reliability
- Failing to parse requirements SHALL fail fast on CLI with a clear message; API callers can validate before passing.

### Usability
- Provide concise README updates showing how to pass requirement context through CLI and API.

## Out of Scope (for this spec)
- No schema changes. We consume the existing `requirements` table.
- No changes to `PathResult` schema.
- No per-node custom error messages for unmet requirements.

## Open Questions
- None at this time.

## References
- API: `navpath/api.py` `find_path()`
- CLI: `navpath/__main__.py`
- Options: `navpath/options.py` `SearchOptions`
- Schema: `docs/tiles_nodes_schema.md` (see `requirements` and `*_nodes.requirement_id`)
