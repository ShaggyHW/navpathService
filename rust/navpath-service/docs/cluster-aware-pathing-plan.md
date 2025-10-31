# Cluster-Aware Pathfinding Refactor Plan

## Summary

The current Rust service always builds an abstract HPA (Hierarchical Path-Finding A*) graph and runs a two-level search even for start/end tiles that lie in the same cluster @rust/navpath-service/src/routes.rs#144-227. The planner dynamically connects virtual start/end nodes to cluster entrances using micro A* searches @rust/navpath-service/src/planner/hpa.rs#29-87 and reconstructs the full tile path by replaying edge blobs or re-running micro A* within clusters @rust/navpath-service/src/planner/hpa.rs#202-305. This refactor shifts to a cluster-aware workflow that avoids the high-level search when the start and goal share a cluster and otherwise navigates between clusters based on precomputed intra/inter connections before filling gaps with micro A*.

## Target Behavior

1. Look up the cluster IDs for the start and goal tiles.
2. If both tiles resolve to the same cluster ID, run the existing 4-direction micro A* constrained to that cluster and return the resulting path immediately.
3. If the tiles belong to different clusters, use the cluster connection graph (including intra-cluster paths, inter-cluster links, and teleports) to find the lowest-cost sequence of entrances that leads from the start cluster to the goal cluster.
4. For each gap between the start tile, intermediate entrances, and the goal tile, run constrained micro A* searches to stitch together the concrete tile path.
5. Preserve existing response semantics (path/actions, `only_actions`) and ensure path segments remain encodable/decodable with the default binary format shown below.

## Required Changes

### 1. Data Access Layer (`db.rs`)

- **Tile → cluster resolution.** Add a query (and cache if needed) that, given `(x, y, plane)`, returns the containing `cluster_id`. The `cluster_tiles` table already holds this mapping @rust/navpath-service/src/db.rs#56-71; expose a direct lookup so callers need not scan all cluster tiles for every request.
- **Cluster adjacency metadata.** Reuse existing getters for entrances, intra connections, inter connections, and teleports @rust/navpath-service/src/routes.rs#156-191. Consider adding helper methods that limit results to clusters relevant to the start/goal to avoid loading whole-plane datasets.
- **Optional caching.** Since cluster membership lookups will happen per request, add an LRU or `OnceCell<HashMap>` cache keyed by `(plane, x, y)` to avoid repeated DB hits. Respect the existing read-only connection pattern so the service stays thread-safe.

### 2. Planner Orchestration (`planner` module)

- **Entry point split.** Introduce a new planner API (e.g., `plan_path`) that first resolves the start/end clusters, decides between the fast intra-cluster path or the cross-cluster workflow, and only invokes the HPA graph building when necessary.
- **Cluster fast-path.** Reuse `find_path_4dir` @rust/navpath-service/src/planner/micro_astar.rs#34-115 with predicates that restrict movement to tiles belonging to the resolved cluster and that respect walkability (existing closure inputs from `routes.rs` can be reused).
- **Cluster graph search.** For cross-cluster cases, build a higher-level graph where nodes represent clusters (or entrances within clusters) and edges correspond to intra/inter connections as stored in the DB. The current `Graph` builder already models entrances and their connectivity @rust/navpath-service/src/planner/graph.rs#62-151; adapt or extend it so we can query the cheapest sequence of entrances between two clusters without running micro A* repeatedly during graph construction.
- **High-level pathfinding.** Either reuse `high_level_astar` @rust/navpath-service/src/planner/hpa.rs#133-200 with adjusted node/edge definitions or create a dedicated Dijkstra search over the cluster graph that returns the ordered entrance sequence needed.
- **Micro bridges.** Maintain helpers similar to `micro_cost_within_cluster` and `append_micro` @rust/navpath-service/src/planner/hpa.rs#202-335 to translate entrance-to-entrance hops (and start/end connections) into concrete tile paths once the cluster route is known.

### 3. Response Assembly & Encoding

- Continue to serialize the response exactly as today (`path`, `actions`, `only_actions`) @rust/navpath-service/src/routes.rs#230-247.
- When storing or emitting precomputed intra-cluster segments, keep using the default blob encoding:

  ```rust
  fn encode_path_blob(path: Vec<(i32,i32)>, plane: i32) -> Vec<u8> {
      let mut out = Vec::with_capacity(path.len() * 12);
      let plane_bytes = plane.to_le_bytes();
      for (x,y) in path {
          out.extend_from_slice(&x.to_le_bytes());
          out.extend_from_slice(&y.to_le_bytes());
          out.extend_from_slice(&plane_bytes);
      }
      out
  }
  ```

- Ensure newly generated micro segments are compatible with `try_decode_default` so reconstruction stays lossless @rust/navpath-service/src/planner/hpa.rs#217-305.

### 4. Routing Layer (`routes.rs`)

- Replace the unconditional `hpa_plan` invocation with the new planner entry point. The route handler should:
  1. Gather only the cluster metadata required by the planner (possibly deferred until after determining start/end clusters).
  2. Pass shared closures for `is_walkable` and `allowed` predicates so the planner can reuse them for micro searches.
  3. Handle planner errors (e.g., missing cluster data, no path) and propagate them as existing HTTP errors.

### 5. Teleports & Multi-Plane Considerations

- Preserve teleport eligibility checks via `RequirementEvaluator` @rust/navpath-service/src/planner/graph.rs#131-148. When start/end are on different planes, ensure the cluster graph search includes teleport edges that bridge planes and that micro bridges handle plane changes gracefully.
- Validate that start/end tiles without cluster membership (e.g., isolated tiles) return a controlled error instead of panicking.

### 6. Testing & Validation

- **Unit tests:**
  - Tile-to-cluster lookup returns expected IDs and handles missing tiles.
  - Planner fast-path returns identical results to current HPA execution for intra-cluster paths.
  - Cross-cluster planner reconstructs paths that match existing HPA outputs on representative scenarios (including teleport cases).
- **Integration tests:** Add cases covering same-cluster, cross-cluster, and teleport-required routes through `/find_path`.
- **Benchmarks (optional):** Compare latencies before/after to confirm the intra-cluster fast-path yields measurable improvements.

### 7. Operational Notes

- Monitor memory usage if caching cluster memberships; consider exposing cache metrics via tracing.
- Document the new behavior in `docs/navpath-service-analysis.md` or the service README so operators understand the cluster-aware flow.

## Open Questions

1. Should cluster membership be derived entirely in-memory (preload all `cluster_tiles`) or lazily with caching? Trade-offs include init cost vs. request latency.
2. Do we need to precompute cluster-level heuristics (e.g., Manhattan distances between cluster centroids) to guide the high-level search, or is pure Dijkstra acceptable?
3. How should we handle tiles that fall outside any cluster (e.g., invalid inputs) — fail fast or fall back to the old HPA behavior?
