# Why `result.json` Lacks Object/NPC Metadata

## What we observed
- The latest `/find_path` debug dump (`result.json`) contains one lodestone action with metadata and then a long series of plain `move` actions.
- None of the later actions include the `metadata` block we expect for objects, NPCs, items, doors, etc.

## How metadata enrichment works today
- After planning, we iterate over every non-move action and, when its `type` maps to a teleport node (`lodestone`, `object`, `npc`, `item`, `ifslot`, `door`), we fetch the corresponding node row from SQLite and inject a `metadata` object into the action.@rust/navpath-service/src/routes.rs#298-491
- Plain `move` actions never receive extra metadata by design—they are serialized straight from the path tiles with only bounds and cost information.@rust/navpath-service/src/routes.rs#255-297

## What the planner produced for this request
- The cluster-aware planner first tries a “same cluster” micro A* when the start and end tiles resolve to the same cluster. That fast path returns the tile list and **no pre-populated actions** (`actions: Vec::new()`).@rust/navpath-service/src/planner/cluster.rs#60-132
- Because that fast-path was taken, the action list passed into the enrichment step contained only synthetic move steps created locally from consecutive tiles. Those actions have no associated teleport node IDs, so the enrichment code had nothing to look up.

## When object/NPC metadata would appear
- Metadata is only attached when the high-level planner emits teleport edges—doors, objects, NPCs, etc. Each teleport edge produces an action with a concrete `node_id`, and the enrichment logic fills in the metadata payload from the corresponding `teleports_*` tables.@rust/navpath-service/src/planner/hpa.rs#236-288 @rust/navpath-service/src/routes.rs#335-484
- If the path never uses an abstract teleport edge (for example, the route is fully walkable within one cluster or requirement checks filter out teleports), the resulting action list is move-only and therefore metadata-free.

## Summary
The service already enriches object/NPC actions, but that code only runs when the planner emits teleport actions. In the captured request the planner solved the route entirely with local movement inside a single cluster, so no teleport actions—and therefore no object/NPC metadata—were generated.
