# RS3 Pathfinder — Plan v2 (SQLite-first, Rust-only)

> **Premise:** With the **SQLite database already populated and verified**, we now focus exclusively on building the runtime, API, and performance layers on top of it. The following plan begins from the completed database foundation and skips earlier setup steps.

---

## 1) Architecture (Rust-only)

### 1.1 Components

- **Builder** (Rust CLI): Reads `worldReachableTiles.db` → constructs a **compact graph snapshot**:
  - **Tile layer:** derive adjacency from `walk_mask` (bits 0..7 for 8 directions; diagonals require both orthogonals; cardinal edges reciprocal by construction).
  - **Action layer:** import rows from `teleports_all`; encode as typed edges with costs + optional requirement handles. **Respect chaining semantics** (`next_node_id`, `next_node_type`) by compiling linear chains into a single macro-edge during build (see §2.2).
  - **Eligibility index:** compile requirement rows to an internal **bitset/tag system** for fast per-profile filtering.
  - Emit: `graph.snapshot` (nodes/edges, metadata, ALT tables) and `tiles.bin` (optional compact clip flags).
- **API** (Rust `axum`): Loads `graph.snapshot` via **mmap + zero-copy** and answers routing queries.
- **(Optional) Watcher**: Monitors the SQLite file or a manifest; on change, runs the Builder and hot-swaps the snapshot.

### 1.2 Data Flow

`worldReachableTiles.db` → **Builder** → `graph.snapshot` (+ `tiles.bin`) → **API** (hot-reload).

---

## 2) Graph Modeling Details

### 2.1 Nodes

- **Tile nodes**: one per walkable `(x,y,plane)` present in `tiles` (i.e., `blocked = 0`).
- **Waypoint/action nodes**: implicit — we keep actions as edges between tile nodes. For teleports with `NULL src_*`, we attach a **profile-gated global source** that can originate from any tile (see §2.2.3). For entries with `NULL dst_*`, the row is a **chain node only** (no spatial destination) and is handled by the builder when flattening chains.

### 2.2 Edges

- **Adjacency edges** from `walk_mask`:

  - Bits → directions mapping preserved; diagonals only if both adjacent cardinals are set.
  - Uniform or terrain-aware costs (start with `1` per tile; later allow terrain multipliers if available in `flag`).

- **Teleport/action edges** from `teleports_*_nodes` (via `teleports_all`):

  - These represent **non-walk connections** between tiles or UI actions that cannot be traversed by walking.
  - `kind` determines the subtype (`door`, `lodestone`, `object`, `npc`, `item`, `ifslot`, etc.).
  - **Cost** comes from the row’s `cost` and is accumulated across chains (see below).

#### 2.2.1 Chaining (`next_node_id`, `next_node_type`)

Some actions are sequences that must execute back-to-back.

- If a row has ``**/**``** set**, then **immediately after** executing this action, the next node **must** be executed, and so on **until a node without a next-node is reached**.
- The Builder traverses these forward links and **compiles the whole sequence into a single macro-edge**:
  - **Source**: the first row’s source (see global rule below for null sources).
  - **Destination**: the final row’s concrete destination tile/area (the last row with a non-null destination).
  - **Cost**: sum of all row `cost`s in the chain.
  - **Requirements**: union/AND of requirements across the chain (caller must satisfy **all**).
  - **Type**: stored as `sequence(kind_1→kind_2→...)` or the dominant terminal kind for telemetry; internally we also preserve the per-step breakdown for debugging.
- **Chain nodes**: if a row’s **destination is NULL**, it is a **pure chain node** (no spatial move on that step). It is **not** emitted as a standalone edge; it only contributes cost/requirements to the macro-edge into which it is flattened.
- **Cycle guard**: the Builder detects cycles in next-node chains and drops/flags them as invalid.

#### 2.2.2 Global teleports (NULL source, non-NULL destination)

- If ``** is NULL** and ``** is NOT NULL**, the action is a **global teleport** usable **from anywhere**.
- Encoding strategies (pick one; default A):
  - **A. Virtual global source**: represent as an edge from a **virtual global node** that is connected to **every tile** with a zero-cost edge; the teleport edge then goes from the virtual node to the destination with its specified cost. At query time, we allow a 0‑cost hop from the current tile to the virtual node.
  - **B. On-demand expansion**: during query, when evaluating actions, inject a candidate edge from the **current tile** directly to the destination if the profile is eligible. (No precomputed fan-out.)
- We adopt **B** to avoid dense precomputation; this keeps the graph sparse and pushes the small branching factor to runtime.

#### 2.2.3 Localized teleports (non-NULL source and destination)

- If both ``** and **`` are present, emit a directed edge from the **source tile/area** to the **destination tile/area** with the given cost and requirements (after chain flattening if applicable).

### 2.3 Requirements / Profile Filtering

- API accepts a `profile.requirements` array (`[{key, value, comparison}]`) and convenience sets (skills/items/flags). The engine builds an **eligibility bitmask** once per request; edges with unsatisfied requirements are skipped. / Profile Filtering
- API accepts a `profile.requirements` array (`[{key, value, comparison}]`) and convenience sets (skills/items/flags). The engine builds an **eligibility bitmask** once per request; edges with unsatisfied requirements are skipped.

### 2.4 Heuristics

- Octile heuristic for local moves; **ALT landmarks** (16–64) chosen among dense hubs (lodestones/city centers) for cross-map tightness.

---

## 3) API

### 3.1 Endpoints

- `GET /health` → `{ version, snapshot_hash, db_manifest, loaded_at }`.
- `POST /route` (world-space default):

```json
{
  "start": {"wx": 3207, "wy": 3422, "plane": 0},
  "goal":  {"wx": 3213, "wy": 3421, "plane": 0},
  "profile": {
    "requirements": [{"key": "lodestone_varrock", "value": true, "comparison": "=="}],
    "skills": {"Agility": 70},
    "items": {"Law rune": 1}
  },
  "options": {"return_geometry": true}
}
```

### 3.2 Response

```json
{
  "time_s": 22.7,
  "length_tiles": 481,
  "segments": [
    {"type": "walk", "polyline": "..."},
    {"type": "lodestone", "label": "Varrock Lodestone", "edge_id": 9123},
    {"type": "walk", "polyline": "..."}
  ],
  "waypoints": [123,456,789],
  "version": "2025-11-07_wrtiles_v1"
}
```

---

## 4) Performance Plan

- **Rust** hot path; **immutable** snapshot loaded via **mmap**.
- **SoA** layout; 32-bit ids; `f32` weights; custom binary heap (optionally bucket/radix for near-uniform costs).
- **Per-request arenas** to avoid alloc churn.
- **ALT** precomputed both directions; landmark count tuned by p95.
- **Concurrency**: one thread per query; no locks in hot path.
- Targets: p50 < 10 ms; p95 < 25 ms; p99 < 50 ms on commodity CPU.

---

## 5) Build & Ops

### 5.1 Builder CLI

```
pathfinder-builder \
  --sqlite ./worldReachableTiles.db \
  --out-snapshot ./graph.snapshot \
  --out-tiles ./tiles.bin \
  --landmarks 32
```

- Outputs carry a **content hash**; API exposes it in `/health`.

### 5.2 Hot Reload

- `SIGHUP` or `/admin/reload` swaps snapshots atomically after verifying checksum.

### 5.3 Config

- `SNAPSHOT_PATH=/data/graph.snapshot`
- `SQLITE_PATH=/data/worldReachableTiles.db` (only needed by builder or if API supports direct-read fallback)
- Optional: `REDIS_URL=redis://...` for memoized waypoint routes and eligibility masks.

---

## 6) Deployment

- **Single node** (Docker Compose): `pathfinder` (Rust API), `builder` (on-change job), optional `redis`, `nginx`/Rust TLS.
- **HA**: run multiple API replicas behind L7 LB; distribute identical snapshots.

---

## 7) Testing

- Golden routes across hubs and known dungeon traversals.
- Property tests: A\* vs Dijkstra parity on samples; diagonal rules honored; requirement gating.
- **Chain flattening tests**: construct mini DB fixtures with 2–4 step chains, mixed with null-destination chain nodes, and assert the Builder emits **one macro-edge** with summed costs and AND’ed requirements.
- **Global teleport tests**: null-source rows should be usable from arbitrary start tiles; confirm branching remains bounded and performance targets hold.
- DB fidelity: spot-check sample rows from `tiles` and `teleports_all` match encoded edges.

## 8) Next Steps

1. Implement chain-flattening in the **Builder** with cycle detection and per-step debug traces.
2. Add runtime expansion for **global teleports** (strategy B) with profile gating.
3. Emit macro-edge metadata (per-step kinds and ids) for observability and debugging.
4. Extend `/health` to expose counts: tiles, adjacency edges, macro-edges, global teleports.
5. Load-test long-range routes; tune ALT landmarks and queue strategy.
6. Wire the **Builder** to `worldReachableTiles.db` and emit first `graph.snapshot`.
7. Choose 32–48 **ALT landmarks** from lodestone/city clusters.
8. Implement **requirement compiler** (DB → edge eligibility bits).
9. Expose `/admin/reload` and `/health` with snapshot/db hashes.
10. Load-test with long-range routes; tune landmark count and queues.

> This plan intentionally **removes spreadsheet ingestion & Postgres** from the critical path. We can add Postgres later for analytics or authoring workflows, but the runtime depends solely on the SQLite-derived snapshot for speed and simplicity.

