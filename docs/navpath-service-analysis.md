# navpath-service Webservice Analysis

## Overview

`navpath-service` exposes an Axum HTTP API that wraps the Rust `navpath-core` pathfinding engine. The primary endpoint `/find_path` accepts a JSON body with start/end tiles and optional requirement key-values, returning a path and action sequence. Cross-plane pathing relies on High-level Pathfinding Abstraction (HPA*) graph data persisted in the SQLite database that ships with navpath.

This document summarizes the relevant modules and surfaces potential causes for cross-plane path failures.

## Components

### `routes::find_path`

Located at `rust/navpath-service/src/routes.rs`, this handler orchestrates request processing:

1. Validates cross-plane requests require a configured database. Without `NAVPATH_DB`, the service rejects the request with `AppError::BadRequest` @rust/navpath-service/src/routes.rs#100-149. The config loader already requires the env var, so the bad-request path should only fire if the state was constructed without DB.
2. Builds a `RequirementEvaluator`, but the current evaluator is unused in `find_path`; teleports are filtered later in `planner::graph`.
3. Opens a read-only `Db` per request and defines `is_walkable` / `allowed` closures. Both rely on `Db::get_tile` and return default `true` when the tile is missing. This fallback may mask missing tiles.
4. Fetches HPA inputs for the start plane and (if different) the end plane: entrances, intra-cluster edges, inter-cluster edges, teleport edges, and teleport requirements.
5. Materializes `cluster_tiles` maps required for micro-A* inside clusters.
6. Calls `planner::hpa::plan` (aliased as `hpa_plan`). The result returns `path_tiles` and `hpa_extra_actions` (teleport annotations etc.).
7. Serializes moves between consecutive tiles as `"move"` actions; teleports come from `hpa_extra_actions`.

### `planner::graph`

`build_graph` creates the high-level graph used by HPA* @rust/navpath-service/src/planner/graph.rs#62-142:

- Entrances sorted deterministically; each becomes a node per plane.
- Adds virtual start/end nodes to graph.
- Adds intra/inter edges using DB data with deterministic ordering.
- Teleport edges: filtered by presence of `src_entrance`/`dst_entrance`, existence in `entrance_index`, and requirement satisfaction via `RequirementEvaluator`. Requirements currently examine only a single record per teleport (no ranges). If teleports lack either entrance or requirement IDs, they are discarded.

### `planner::hpa`

`plan` builds micro connections and runs high-level Dijkstra-like search @rust/navpath-service/src/planner/hpa.rs#29-87. Key behaviors:

- Micro A* edges from start -> entrances exist only on start plane; reverse for entrances -> end with end plane.
- If the micro search or `cluster_tiles` lacks entries, extra edges are omitted, which can disconnect the graph.
- Teleport actions are simply appended as single steps to destination entrance tile, assuming the base graph contains them.
- `find_path_4dir` in `planner::micro_astar` refuses start/end on different planes @rust/navpath-service/src/planner/micro_astar.rs#34-115.

### Database layer

`Db::list_abstract_teleport_edges_for_planes` retrieves teleports that touch either of the two requested planes @rust/navpath-service/src/db.rs#231-250. The SQL joins cluster entrances on both sides and only returns rows where both endpoints belong to one of the two planes. Teleports with endpoints on planes outside the requested pair are filtered out even if they provide an intermediate hop.

`Db::get_tile` returns `Option<TileRow>`; the service treats `None` as walkable (`true`). Missing tiles lead to unconstrained micro search.

## Potential Cross-plane Path Issues

1. **Plane Pair Filtering of Teleports**: `list_abstract_teleport_edges_for_planes` restricts teleports to those whose source and destination entrances lie in either plane S or plane E. Multi-hop cross-plane paths requiring intermediate plane detours (S -> plane X -> E) are excluded. If the dataset represents cross-plane movement through intermediate planes, HPA will lack these teleports and fail. Consider broadening the query or running iterative plane inclusion based on teleport graph reachability.

2. **Teleport Entrances Absent in Entrances List**: Teleports require both `src_entrance` and `dst_entrance`. If a teleport references an entrance not returned by `list_cluster_entrances_by_plane`, `build_graph` drops it. This can happen if the teleport's plane differs from start/end plane but is reachable. Currently we only load entrances for start/end planes. Entrances on intermediary planes are missing, leading to dropped teleports.

3. **Cluster Tiles Missing for Teleport Destinations**: `cluster_tiles` is populated only for clusters referenced by `entrances`. If teleport destination cluster is not among the start/end plane entrances, micro reconstruction may fail to append proper tiles, causing the path to degrade to a single tile append (due to fallback in `append_micro`). In the worst case, the path may miss required inter-plane moves.

4. **Requirement Evaluator Unused for Teleports**: In `routes::find_path`, we build `RequirementEvaluator` but never pass requirement context into the graph builder beyond teleports. However, `planner::graph` uses the evaluator correctly when building teleports. No direct issue here, but worth noting redundant variables.

5. **Walkability / Allowed Predicates**: Both allow fallback `true` when DB lookup fails. If teleports lead to tiles not present in DB, micro A* may attempt to traverse nonexistent tiles. Combined with missing cluster tiles, this may produce nonsensical paths.

## Suggested Investigation Steps

1. **Inspect Teleport Data**: Verify that the cross-plane teleport edges exist with `src_entrance`/`dst_entrance` pointing to entrances on start/end planes. If intermediate planes are necessary, adjust data loading to include those planes.
2. **Expand Plane Coverage**: Modify `find_path` to gather entrances/intra/inter/teleports for all planes reachable via teleports between start and end. This might require BFS over teleport graph.
3. **Enforce Tile Availability**: Change `allowed`/`is_walkable` to return false when `get_tile` yields `None`. This forces the code to surface missing data instead of silently treating missing tiles as walkable.
4. **Add Diagnostics**: Log when teleports are discarded due to missing entrances or requirement failures; include plane IDs for debugging cross-plane requests.

## Next Steps

- Validate hypotheses by querying the SQLite DB for teleport edges spanning the affected planes.
- Reproduce the cross-plane request in integration tests covering multi-hop teleport scenarios.
- Consider adjusting SQL queries and in-memory data gathering to include all necessary planes.
