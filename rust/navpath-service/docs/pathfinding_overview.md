# Navpath Service Pathfinding Flow

This document explains how the `/find_path` endpoint processes a request using the database-backed HPA* flow. It references code from the Rust service and includes a diagram showing the control flow.

## High-Level Flow

- **HTTP layer**: `routes::find_path` handles POST requests to `/find_path` and optionally parses the `only_actions` query flag.
- **Requirement evaluation**: Teleport requirements from the request body are converted into a `RequirementEvaluator` for gating teleport edges.
- **Database usage**:
  - The service requires `NAVPATH_DB` and opens the SQLite database for tile and hierarchy data.
  - The DB supplies entrances, cluster connectivity, and abstract teleport edges for both the start and end planes @rust/navpath-service/src/routes.rs#150-188.
- **Planner**:
  - The hierarchical planner (HPA*) is always executed, combining cluster-level routing with micro A* refinements @rust/navpath-service/src/planner/hpa.rs#29-214.
- **Response shaping**: The final tile path is serialized alongside movement and teleport actions; optional query flags can return `actions` only @rust/navpath-service/src/routes.rs#238-253.

## Detailed Diagram

```mermaid
flowchart TD
    A[HTTP POST /find_path] --> D[Open read-only DB]
    D --> E[Load cluster entrances/intra/inter for start & end planes]
    D --> F[Load teleport edges spanning both planes]
    D --> G[Load teleport requirements]
    E --> H[Build GraphInputs]
    F --> H
    G --> H
    H --> I[HPA plan: build graph + run HL search]
    I --> J[Micro A*: connect start & end within cluster tiles]
    J --> K[Collect tile path + teleport actions]
    K --> R[Format response (path + actions)]
```

## HPA* Sequence

1. **Graph construction**: `planner::graph::build_graph` prepares abstract nodes for cluster entrances, virtual start/end nodes, and adds edges (intra-cluster, inter-cluster, teleports) gated by the requirement evaluator @rust/navpath-service/src/planner/graph.rs#62-143.
2. **Dynamic micro edges**: When building edges from the virtual start and to the virtual end, micro A* edges are added only on the corresponding plane so cross-plane teleports remain valid @rust/navpath-service/src/planner/hpa.rs#48-78.
3. **High-level search**: `high_level_astar` runs a Dijkstra-style search over the abstract graph to produce a sequence of nodes @rust/navpath-service/src/planner/hpa.rs#132-199.
4. **Tile reconstruction**: For each abstract hop, `reconstruct_tiles_and_actions` inlines either stored path blobs, additional micro A* runs, or teleport actions to emit the final tile list and action array @rust/navpath-service/src/planner/hpa.rs#216-304.

## Requirements & Teleports

- Request body `requirements` are parsed into key/value/comparison tuples. The `RequirementEvaluator` checks them against teleport requirement records so only satisfied teleports remain in the graph @rust/navpath-service/src/requirements.rs.
- Teleport actions are appended to the action list when the abstract path uses a teleport edge, preserving both movement and fast-travel steps in the response @rust/navpath-service/src/planner/hpa.rs#269-280.

## Configuration Requirement

The service refuses to start without `NAVPATH_DB` set and accessible. `/readyz` returns `ready:false` if the DB cannot be opened or minimally queried.

## Debugging Tips

- Enable structured logs via `RUST_LOG=info,navpath_service=debug` to see the planner choice, algorithm timings, and any serialization errors @rust/navpath-service/src/routes.rs#242-249.
- Set `NAVPATH_DEBUG_RESULT_PATH` to write the last JSON response to disk for inspection @rust/navpath-service/src/routes.rs#235-239.
