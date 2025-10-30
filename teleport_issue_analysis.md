# Teleport Usage Investigation

## Summary
- `/find_path` currently runs a direct tile-level A* search and never consults teleport edges, so it always returns walking routes even when teleport requirements are satisfied.
- The endpoint instantiates a `RequirementEvaluator` but explicitly drops it, leaving teleport gating unused in the live request flow.
- Teleport edges are only honored inside the hierarchical planner modules, which are not invoked by the HTTP handler today.

## Reproduction
```bash
curl -s "http://127.0.0.1:8080/find_path?only_actions=true" \
  -H 'content-type: application/json' \
  -d '{"start":{"x":2899,"y":3540,"plane":0},"end":{"x":3292,"y":3191,"plane":0},"requirements":[{"key":"coins","value":"30"},{"key":"varbit_28", "value":1}]}'
```
The response contains only walk actions; no teleport action is emitted.

## Findings

1. **Endpoint ignores teleport evaluator.** The `/find_path` handler constructs a `RequirementEvaluator` and immediately discards it (`let _ = evaluator`), so caller-supplied requirements never influence planning. The rest of the handler proceeds with tile-level search only @rust/navpath-service/src/routes.rs#109-152.
2. **Route falls back to plain micro A*.** After the unused evaluator, the handler calls `find_path_4dir` over world tiles, which can only walk between start and end; teleports are impossible in that algorithm @rust/navpath-service/src/routes.rs#142-151.
3. **Teleport support exists only in HPA planner.** Teleport edges are filtered by requirements in the abstract graph builder and then converted into teleport actions during hierarchical path reconstruction @rust/navpath-service/src/planner/graph.rs#121-140 @rust/navpath-service/src/planner/hpa.rs#29-85. Because the HTTP route never invokes the HPA planner, this logic is currently unreachable from the service.

## Next Steps
1. Wire `/find_path` to the hierarchical planner so that teleport edges (and other abstract connections) are considered.
2. Once HPA is integrated, ensure requirements from the request body populate the evaluator passed into `plan`, allowing teleports to trigger when satisfied.
3. Add an integration test covering a teleport-enabled request to prevent regressions.
