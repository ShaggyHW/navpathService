# Performance Improvement Plan

## Executive Summary
The project has already implemented significant architectural optimizations (Static Graph, SearchContext reuse, Eligibility Masks). However, there are critical remaining opportunities to improve response time and throughput. The most significant findings are the **disabled Octile heuristic**, **CPU-bound blocking operations in async handlers**, and **unnecessary allocations in the hot path**.

## 1. Critical Fixes (High Impact)

### 1.1. Enable Octile Heuristic
**Severity:** Critical
**Impact:** Large speedup for local pathfinding.
**Current State:** `engine_adapter.rs` uses `DummyCoords` which returns `(0,0,0)` for all nodes. This effectively disables the Octile heuristic, forcing A* to rely solely on Landmarks (ALT). If landmarks are not perfectly placed or for local navigation, A* degrades towards Dijkstra.
**Fix:**
1.  Create a `SnapshotCoords` struct that holds references to `nodes_x`, `nodes_y`, `nodes_p` slices from the `Snapshot`.
2.  Implement `OctileCoords` for `SnapshotCoords`.
3.  Pass this to `astar` instead of `DummyCoords`.

### 1.2. Offload CPU-Bound Tasks
**Severity:** High
**Impact:** Massive throughput improvement under load.
**Current State:** The `route` handler in `routes.rs` calls `engine_adapter::run_route_with_requirements` directly in the `async` function. This blocks the Tokio worker thread, preventing it from handling other I/O (like accepting new connections) while pathfinding runs.
**Fix:**
1.  Wrap the search call in `tokio::task::spawn_blocking`.
2.  Ensure `Arc` clones are lightweight and moved into the blocking task.

## 2. Micro-Optimizations (Medium Impact)

### 2.1. Optimize Global Teleport Injection
**Severity:** Medium
**Impact:** Reduced allocation per request.
**Current State:**
- `run_route_with_requirements` filters global teleports into a new `Vec` every request.
- It creates a `Box<dyn Fn...>` closure which captures this vector.
- `astar` calls this closure, which clones the vector again.
- `astar` sorts the vector inside the hot loop.
**Fix:**
1.  Change `EngineView.extra` to be a `&[(u32, f32)]` slice or a concrete iterator type, removing `Box<dyn Fn>`.
2.  Filter eligible globals into a reusable buffer (e.g., in `SearchContext` or a separate pool) to avoid `Vec` allocation.
3.  If possible, keep globals pre-sorted or sort them once outside `astar`.

### 2.2. Zero-Allocation Requirement Masking
**Severity:** Low/Medium
**Impact:** Reduced allocator pressure.
**Current State:** `run_route_with_requirements` allocates `client_pairs: Vec<(String, ClientValue)>` to bridge the API request to the internal mask builder.
**Fix:**
1.  Modify `build_mask_from_u32` (or create a variant) that accepts an iterator of the raw `RequirementKV` from the request, avoiding the intermediate `client_pairs` vector.

### 2.3. Optimize JSON Response Building
**Severity:** Low
**Impact:** Lower latency for `return_geometry` requests.
**Current State:** `routes.rs` constructs complex `serde_json::Value` trees for actions and geometry. This involves many small allocations.
**Fix:**
1.  Define strongly-typed `Serialize` structs for the Action/Geometry response objects instead of using the untyped `json!` macro.
2.  This allows `serde` to write directly to the output buffer without building an intermediate DOM.

## 3. Implementation Steps

### Step 1: Async Offloading
- Modify `routes.rs`:
  ```rust
  // Extract data needed for search
  let snap = cur.snapshot.clone();
  let neighbors = cur.neighbors.clone();
  let globals = cur.globals.clone();
  let sid = ...;
  let gid = ...;
  let client_reqs = ...;

  // Spawn blocking
  let res = tokio::task::spawn_blocking(move || {
      engine_adapter::run_route_with_requirements(...)
  }).await.unwrap();
  ```

### Step 2: Enable Octile
- In `engine_adapter.rs`:
  ```rust
  struct SnapshotCoords<'a> {
      x: &'a [i32],
      y: &'a [i32],
      p: &'a [i32],
  }
  impl<'a> OctileCoords for SnapshotCoords<'a> { ... }
  ```
- Update `run_route_with_requirements` to instantiate this and pass it to `astar`.

### Step 3: Optimize Globals
- Refactor `EngineView` to accept `extra_edges: &[(u32, f32)]`.
- Remove the closure.
- In `astar`, simply iterate this slice.

### Step 4: Reduce Allocations
- Implement struct-based serialization for response helpers in `routes.rs`.

## 4. Advanced Optimizations (Phase 4)

### 4.1. Landmark Memory Layout Transpose
**Severity:** High (Cache Misses)
**Impact:** Significant speedup for ALT heuristic calculation.
**Current State:** Landmarks are stored in "Landmark-Major" order (`[L0_Node0...NodeN, L1_Node0...]`). Computing `h(n)` requires accessing `L0_NodeN`, `L1_NodeN`... which are spaced `Nodes` bytes apart (e.g., 4MB). This causes a cache miss for every landmark iteration.
**Fix:**
1.  Update `navpath-builder` to write landmark tables in "Node-Major" order (`[Node0_L0...Lm, Node1_L0...]`).
2.  Update `navpath-core` heuristics to access contiguous memory for `h(n)`.

### 4.2. SIMD Heuristics
**Severity:** Medium
**Impact:** Faster heuristic computation.
**Current State:** Heuristic loop calculates max over all landmarks sequentially.
**Fix:**
1.  Once layout is transposed (see 4.1), use SIMD (e.g., `std::simd` or loop vectorization) to compute 8 or 16 landmark values in parallel.

### 4.3. Compiler Optimizations
**Severity:** Medium
**Impact:** 10-20% global speedup.
**Current State:** `navpath-service` does not have specific release profile settings (LTO, codegen-units).
**Fix:**
1.  Add `[profile.release]` to the workspace `Cargo.toml`.
2.  Enable `lto = true`, `codegen-units = 1`, `panic = "abort"`.

## 5. Architectural Optimizations (Phase 5)

### 5.1. Sparse Topology (Jump Point Graph)
**Severity:** High (Architecture Change)
**Impact:** Massive reduction in graph size (nodes/edges), potentially 90%+ smaller.
**Concept:**
As suggested, in uniform 9x9 areas (and other open regions), we do not need to store or visit every single tile. We only need to store "decision points" (Jump Points) where optimal paths diverge.
**Plan:**
1.  **Builder:** Implement a pruning pass.
    - Identify "Jump Points": Corners, Obstacle boundaries, Teleport sources/destinations.
    - Construct a visibility graph connecting these Jump Points.
    - Discard all intermediate "pass-through" nodes from the Snapshot.
2.  **Service:** Handle off-graph queries.
    - Since `start` and `goal` might be pruned "pass-through" tiles, `run_route` must dynamically connect them to the graph.
    - Perform a local search/raycast from `start` to finding the nearest visible Jump Points to enter the graph.
    - Do the same for `goal`.
3.  **Benefit:** Drastically reduces `N` (nodes) and `E` (edges), making A* much faster and `SearchContext` much smaller (fitting in L2/L3 cache).


