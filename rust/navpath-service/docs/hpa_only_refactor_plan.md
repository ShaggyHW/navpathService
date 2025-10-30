# Refactor Plan: Always Use DB/HPA Pathfinding

This guide documents the changes required to remove the micro A* fallback and ensure the `/find_path` endpoint always executes the database-backed HPA* flow. The diagram below shows the desired steady-state control flow.

```mermaid
flowchart TD
    A[HTTP POST /find_path] --> B[Open read-only DB]
    B --> C[Load cluster entrances/intra/inter for start & end planes]
    B --> D[Load teleport edges spanning both planes]
    B --> E[Load teleport requirements]
    C --> F[Build GraphInputs]
    D --> F
    E --> F
    F --> G[HPA plan: build graph + run HL search]
    G --> H[Micro A*: connect start & end within cluster tiles]
    H --> I[Collect tile path + teleport actions]
    I --> J[Format response (path + actions)]
```

## Current Behavior Snapshot

- `find_path` currently short-circuits to micro A* when `db_path` is absent, and only runs HPA* when a database connection succeeds. See `if let Some(ref db) = db_for_hpa { ... } else { ... }` in `routes.rs` @rust/navpath-service/src/routes.rs#147-235.
- Micro A* uses `find_path_4dir` with closures `allowed` and `is_walkable` defined earlier in the handler @rust/navpath-service/src/routes.rs#205-232.

## Target Changes

1. **Require `NAVPATH_DB` configuration**
   - Update `Config::from_env` to error if `NAVPATH_DB` is missing.
   - Adjust service bootstrap (e.g., `main.rs`) to propagate that error so the service refuses to start without a DB.

2. **Remove micro A* fallback**
   - In `find_path`, delete the `else` block that invokes `find_path_4dir` and treat a missing DB as an internal error.
   - Simplify the planner invocation so `db_for_hpa` is always `Some` and unwraps are safe after the configuration guard.

3. **Tighten readiness checks**
   - Ensure `/readyz` returns `ready: false` if the DB cannot be opened or the required tables are missing.

4. **Update tests**
   - Remove or rewrite integration/unit tests that expect the micro A* fallback.
   - Add coverage confirming that the handler rejects requests when the DB is unavailable.

5. **Documentation**
   - Replace diagrams that show the fallback branch (including `docs/pathfinding_overview.md`) with the simplified flow.
   - Note the stricter configuration requirement in `README.md`.

## Implementation Notes

- After removing the fallback, it may be useful to expose a specific error message (e.g., `500 Missing database configuration`) when `NAVPATH_DB` is unset instead of the current implicit branch.
- Consider adding a startup migration or validation step so missing tables fail fast before serving traffic.
