# Teleport Next-Node Chain Analysis

## Background

A sample `/find_path` response shows an NPC teleport action with `metadata.db_row.next_node_type` and `next_node_id` populated, but the returned `actions` array stops after the first node.@/home/query/Dev/navpathService/result.json#9386-9445 The service needs to append all follow-up teleport steps that form the chain referenced by these fields.

## Current Implementation

1. **Teleport actions emitted during reconstruction** – When the HPA reconstructor encounters a teleport edge it creates a single action JSON object and pushes it into `actions`; the payload only includes the edge/node that triggered the hop.@rust/navpath-service/src/planner/hpa.rs#237-289
2. **Response assembly** – The route handler maps non-move actions back onto the path hops and then enriches them with DB metadata prior to responding.@rust/navpath-service/src/routes.rs#260-399
3. **Metadata enrichment** – For each teleport `kind` the handler loads a row from the corresponding `teleports_*_nodes` table via the helpers in `Db`.@rust/navpath-service/src/db.rs#309-395 The enrichment copies database columns—including the `next_node_type`, `next_node_id`, and `requirement_id`—into the action metadata.@rust/navpath-service/src/routes.rs#305-363

At no point does the planner or route layer traverse the `next_node_*` chain. Each teleport action therefore represents only the first step, leaving callers to infer or manually execute the remaining nodes.

## Teleport Node Chain Data

Every teleport node table encodes a successor pointer and optional gating requirement:

- NPC nodes: `teleports_npc_nodes` contains destination bounds, action text, and `next_node_type`/`next_node_id` pairs.@rust/navpath-service/src/db.rs#338-352 @rust/navpath-service/src/db.rs#569-689
- Additional teleport tables (objects, ifslots, items, doors, lodestones) expose the same linkage columns via the existing getters.@rust/navpath-service/src/db.rs#309-395 @rust/navpath-service/src/db.rs#569-720

This structure supports arbitrarily long chains (e.g., talk to NPC → click interface → interact with object) with potential requirement changes between steps.

## Gap Analysis

- The planner returns only the originating teleport action; successor nodes are not resolved or appended to the path/actions list.
- Requirement evaluation is performed only on the teleport edge’s `requirement_id` in `build_graph`; child nodes’ requirements are never checked, so gated steps could be emitted even when unsatisfied.
- There is no loop/termination safeguard if a chain references itself or forms a cycle.

## Recommended Next Steps

1. **Iterative node expansion**
   1. After enriching an action, if `next_node_type`/`next_node_id` are set, iteratively fetch rows from the corresponding `teleports_*_nodes` table.
   2. For each successor, emit a new action entry capturing its concrete action (e.g., interface click) and append it to the response in chain order.
   3. Stop when a node lacks a successor or when a previously seen `(type,id)` pair repeats (cycle guard).

2. **Requirement checks per node**
   - Reuse `RequirementEvaluator::satisfies_all` against the successor node’s `requirement_id` (if present) before appending the step to ensure the caller meets all constraints.@rust/navpath-service/src/requirements.rs#10-35

3. **Bounds/tiles handling**
   - Successor nodes often provide destination min/max; convert them to `Bounds` using the existing serialization helpers so downstream consumers receive consistent geometry data.
   - If a successor moves the player (e.g., NPC teleports to another interface), decide whether it should mutate the `path` list or only append to `actions`; document whichever policy is chosen.

4. **Testing**
   - Add integration coverage where a multi-step teleport chain exists (NPC → IF slot → object) and assert that the final response includes every step in order with correct metadata.
   - Include negative tests where chain requirements are unsatisfied to verify the steps are skipped.

## Open Questions

1. Should the chain-expansion logic live in the planner (during reconstruction) to preserve action ordering, or in the route handler after enrichment where DB access already occurs?
2. How should movement between nodes in the chain update the primary `path` tile list, especially when the successor defines a distinct destination region?
3. Do any chains reference node types without existing getters (e.g., `teleports_spell_nodes`); if so, new DB accessors will be required.

## Proposed Implementation Plan (Next-Node Chain Expansion)

### Scope

- **Where**: Implement in the route layer during action enrichment in `find_path` after we have `hpa_extra_actions` and the `Db` handle. This avoids DB in planner loops and reuses existing getters and `RequirementEvaluator`.
- **What**: For each teleport action already produced by HPA, append zero or more concrete successor actions by following `metadata.db_row.next_node_type` and `next_node_id` until termination.

### Behavior

- Preserve existing HPA action ordering. Each chain’s steps are appended immediately after the originating action.
- Use the same action shape keys already present: `type`, `from`, `to`, `cost_ms`, `node { type, id }`, and `requirement_id`. For successor steps without an edge id, set `edge_id` to `null`.
- Include a `metadata.db_row` snapshot for each successor using the same per-kind structure we already produce for the first node.
- Requirement gating: if a successor node carries `requirement_id` and it is not satisfied by the request’s provided `requirements`, skip that successor and terminate the chain.

### Node kind mapping

- Supported kinds mirror existing enrichment code: `"object"`, `"npc"`, `"item"`, `"ifslot"`, `"door"`.
- Getter mapping:
  - `object` → `Db::get_object_node`
  - `npc` → `Db::get_npc_node`
  - `item` → `Db::get_item_node`
  - `ifslot` → `Db::get_ifslot_node`
  - `door` → `Db::get_door_node`

### Algorithm (per originating teleport action)

1. Extract `curr_type = metadata.db_row.next_node_type` and `curr_id = metadata.db_row.next_node_id`. If either missing → done.
2. Initialize `seen = set()` and `max_steps = 32` as safety guards.
3. Let `prev_to = act.to` (bounds) to seed the first successor’s `from`.
4. Loop while `curr_type`/`curr_id` present and steps < `max_steps` and `(curr_type,curr_id)` not in `seen`:
   - Add `(curr_type,curr_id)` to `seen`.
   - Fetch row using the mapped getter. If not found → stop.
   - If `row.requirement_id` present and `!evaluator.satisfies_all(row.requirement_id)` → stop.
   - Build `to_bounds` from row destination columns:
     - Prefer `dest_*` when available; else fall back to `orig_*` if needed (conservative default).
   - Construct successor action:
     - `type = curr_type`
     - `from = prev_to`
     - `to = to_bounds`
     - `cost_ms = row.cost.unwrap_or_default()`
     - `node = { type: curr_type, id: curr_id }`
     - `edge_id = null`
     - `requirement_id = row.requirement_id`
     - `metadata.db_row = <row fields mirrored like existing enrichment for this kind>`
   - Append the action to the `actions` list immediately after the originating action and any prior successors.
   - Update `prev_to = to_bounds`.
   - Set `curr_type = row.next_node_type`, `curr_id = row.next_node_id` for the next iteration.
5. Terminate when successor missing, requirement unsatisfied, cycle detected, getter missing, or step limit reached.

### Data and serialization details

- Bounds: use `crate::serialization::Bounds::from_min_max_plane(min_x,max_x,min_y,max_y,plane)` equivalent helper to construct `from`/`to` consistently with existing actions.
- Determinism: keep insertion order stable; do not reorder across different chains. Within one chain, order is strictly traversal order.

### Limits and safeguards

- `max_steps = 32` to prevent runaway chains.
- Cycle guard by tracking visited `(type,id)` pairs.
- Missing or malformed successor fields abort only that chain, not the whole request.

### Testing Plan

- Unit/integration tests under `navpath-service`:
  - Multi-step chain: NPC → IF slot → Object. Verify all appended actions with correct `type`, `node.id`, `requirement_id`, and bounds.
  - Requirement-gated step: provide requirements that do and do not satisfy; ensure unsatisfied halts expansion.
  - Cycle protection: craft a two-node cycle and assert termination at the guard without panic.
  - Missing successor: ensure graceful stop.
  - Only-actions mode: `?only_actions=true` returns actions including chain steps.

### Implementation Tasks

- Extend `routes.rs` enrichment block to perform chain expansion per action after kind-specific metadata insertion.
- Factor small helper: `expand_next_node_chain(db, evaluator, first_action) -> Vec<serde_json::Value>` to keep `find_path` readable.
- Add tests and sample fixture capturing a real chain; update docs with examples once verified.
