# NavPath Full Optimization Audit — Pathfinding Latency, Snapshot Build, and the 4M-Tile Target

**Date:** 2026-07-05
**Scope:** entire repo as of branch `NewOptimizations` working tree (uncommitted changes included). Existing docs in `docs/` were deliberately ignored; this is a fresh analysis.
**Method:** 10 specialist analysis agents (one lens each: A\* core, heuristics/landmarks, neighbors/eligibility, snapshot format, builder pipeline, SQLite ingest, service runtime, memory/data-structures, 4M scalability, toolchain), every finding adversarially verified against the actual code by a second agent, then a completeness critic dispatched 3 follow-up investigations (unbounded worst case, bidirectional search, grid-structure techniques). 27 agents total; 87 findings confirmed, 14 gap findings, 1 refuted, 2 flagged uncertain. All claims below cite `file:line` and were checked against the working tree; measured numbers were taken on this machine (Ryzen 9 7950X, 32 threads, 30 GB RAM).

---

## 0. Measured baseline

| Metric | Value |
|---|---|
| Tiles / nodes | 1,121,977 (target: 4,000,000+, ≈3.6×) |
| Walk edges | 7,531,920 (avg degree 6.71; only two weights exist: 300 and 424.26 ms) |
| Macro edges (doors/teleport chains) | 957 |
| Global (sourceless) teleports | 124 entries in the snapshot meta (measured); the DB holds ~133 sourceless chains (lodestone/item/ifslot) — a few drop out during chain flattening |
| Fairy rings | 53 |
| Landmarks | 64 — **all of them are node ids 0..63, a single ~23×6-tile cluster at one map corner** |
| Snapshot size | 683,717,235 B — **84.0% is the two ALT tables (2 × 287.2 MB)**; walk edges 90.4 MB (13.2%); everything else < 3% |
| Snapshot at 4M (naive) | ≈ 2.43 GB (ALT alone ≈ 2.05 GB) |
| Full build time | **11.64 s** with `--landmarks 64`; **1.03 s** with `--landmarks 0` → the 128 serial Dijkstras are **91% of build wall time**, single-threaded |
| Builder peak RSS | 1,525 MB (≈ 5 GB projected at 4M — OOM risk on modest hosts) |
| Tile SQL scan | 514 ms with `ORDER BY plane,y,x` vs 34 ms without (forced TEMP B-TREE; PK is `(x,y,plane)`) |
| Tile DB | 58 MB, of which 95% is the tiles table stored **three times** (table + PK autoindex + an exact-duplicate index) |
| Walk-only components | 490 (mega-component = 1,087,662 nodes = 96.9%) |
| Per-request search state | 13 B/node = 14.6 MB per thread today, 52 MB at 4M, on an unbounded 512-thread blocking pool |

### Snapshot section map (measured from the v7 header)

| Section | Size | Share | Notes |
|---|---|---|---|
| `lm_fw` + `lm_bw` | 574.4 MB | 84.0% | f32, node-major, 64 landmarks |
| `walk_src/dst/w` | 90.4 MB | 13.2% | COO triples; fully derivable from walk_mask |
| `nodes_ids/x/y/plane` | 18.0 MB | 2.6% | `nodes_ids` is the identity sequence — dead data |
| macro + meta blob + req + fairy | ~0.9 MB | 0.1% | meta blob contains one 113 KB “global” JSON entry |

**The two root causes of slow pathfinding today:** (1) the heuristic is effectively broken (clustered landmarks + a 300×-underscaled octile term), so long searches degenerate toward Dijkstra over a 1.1M-node graph; (2) the expansion loop does ~20× more work per pop than necessary (124 global-teleport edges merged into every pop, no stale-pop skip, scattered state). **The two root causes of slow builds:** 128 serial Dijkstras (91% of wall time) and a writer that materializes the whole file in RAM 4 bytes at a time.

---

## 1. Pathfinding latency — algorithmic fixes (highest impact)

### 1.1 Fix landmark selection (the single biggest lever) — `high impact / medium effort`
`rust/navpath-builder/src/main.rs:459-463` picks landmarks as `(0..n)`. Node ids follow `ORDER BY plane,y,x` (`load_sqlite.rs:40`), so all 64 landmarks sit in one tiny corner pocket; verified against the live snapshot, they span x 1233–1255, y 84–89 on a map spanning x 995–5731, y 84–10261. 64 near-identical columns ≈ **one** landmark, so the ALT bound `max(d(L,g)−d(L,u), d(u,L)−d(g,L))` is near zero for most pairs and A\* runs near-Dijkstra. This simultaneously wastes the 574 MB of tables.

**Fix:** farthest-point selection (pick a random reachable seed; repeatedly add the node maximizing min graph distance to the chosen set, reusing the existing Dijkstra in `landmarks.rs`; seed at least one landmark per plane/major component so non-plane-0 rows aren’t all INFINITY). Then **reduce the count**: 16–24 well-spread landmarks strictly dominate 64 clustered ones. Keep `ACTIVE_LANDMARKS = 8` (re-benchmark 4 vs 8 after).
**Expected:** 5–20× fewer expansions on long routes (est. up to 10–100× on cross-map queries), and the ALT section shrinks 2.7–4× as a free side effect. Rebuild-only; no format change.

### 1.2 Relax global teleports once from the start, not at every pop — `high / small`
`search.rs:247-283` merges the full `extra.global` slice (all eligible globals, up to 124) into the neighbor stream of **every** popped node. A global edge costs the same from every source, so `g(u)+c ≥ g(start)+c = c`: relaxing them anywhere after the start is provably dead work (min walk weight 300 vs jitter < 0.1, so the ordering can never flip). With avg walk degree 6.7, up to ~95% of merge iterations per pop are these dead relaxations — ~13M wasted iterations per 100k-pop request.

**Fix:** after `ctx.set_g(start, 0.0)` (`search.rs:224`), push each `(dst, cost)` with parent = start; remove `extra.global` from the per-pop merge (keep the per-node fairy path). Results are bit-identical apart from ≤0.1 ms jitter tie-breaks.
**Expected:** removes 40–70% of expansion-loop iterations; 1.5–3× on teleport-enabled requests.

### 1.3 Add the stale-pop skip (and delete `in_open`) — `medium / small`
The loop pops lazy-deletion duplicates and fully re-expands them: there is no `if gcur > ctx.get_g(u) { continue; }` after the pop (`search.rs:236-239`). Grid A\* typically sees 10–40% duplicate pops; each one re-runs the whole neighbor + (today) globals merge. Ironically `in_open` is written on every push/pop precisely to support this check, but `is_in_open` has **zero call sites** — it is pure dead cache traffic plus 1 MB/context (4 MB at 4M). The builder’s own Dijkstra already does this correctly (`landmarks.rs:90`).

**Fix:** one line after the pop; then delete `in_open`, `set_in_open`, `is_in_open`, and the reset in `set_g`.
**Expected:** 10–30% fewer expansions, compounding with 1.2.

### 1.4 Repair heuristic admissibility (correctness + speed)
Three compounding defects:

- **Octile is summed with ALT and 300× under-scaled.** `search.rs:232,295` computes `h = h_active + octile`, but `octile()` returns tile units (`heuristics.rs:129-138`) while all edge weights are milliseconds (`graph.rs:100`: `cost * 300.0`). Sum of two lower bounds is not a lower bound (inadmissible, bounded ~0.3% today), and at 1/300 scale it contributes no guidance while costing 6 bounds-checked mmap coord reads per improvement. Naively scaling by 300 is also wrong: a 2400 ms lodestone undercuts `300*octile` beyond ~8 tiles. **Recommendation: delete the octile term** once 1.1 lands (ALT dominates it). If a geometric bound is ever wanted: `max(ALT, min(300*octile, cheapest_eligible_teleport_cost))`.
- **The ALT graph is missing edges the search uses.** `compute_alt_tables` (`landmarks.rs:19-39`) covers walk+macro only; at query time the service injects 124 globals + fairy hops (`engine_adapter.rs:446-464`). True distances can therefore be smaller than the “lower bounds” → overestimation → suboptimal paths that masquerade as speed (routes biased away from teleports). **Fix (cheap, exact):** per query compute `cap = min over injected edges t of (w_t + h_active(dst_t))` once, then use `h'(u) = min(h_active(u), cap)` — provably admissible/consistent on the augmented graph; O(#globals+#rings) per query, one extra f32 `min` in the loop. Alternatively bake globals/fairy into the ALT build (seed forward Dijkstras with global dsts; cap the reverse pass).
- **INFINITY poisoning.** Unreachable pairs leave `f32::INFINITY` in the tables (`landmarks.rs:42-43`); `select_active` sorts inf-scored landmarks first and `h_active` yields inf/NaN, silently degrading affected queries to Dijkstra ordering. Fix with a finite sentinel (or the u16 saturation in §3.1).

### 1.5 Fix f-tie-breaking to prefer HIGH g — `medium / small`
`Key::cmp` (`search.rs:49-53`) reverses the g comparison so smallest g pops first on f-ties. On a uniform-cost grid with a weak heuristic, f-plateaus are huge and low-g-first explores them breadth-first. Removing the `.reverse()` on the g leg (one line) is the standard fix; up to 2× fewer expansions on plateau-heavy unseeded queries.

### 1.6 Multi-source search for virtual starts — `high / small-medium`
`run_route_with_requirements_virtual_start` (`engine_adapter.rs:558-583`) runs a **complete A\* per eligible global teleport** — up to 124 sequential full searches (each with `ctx.reset`, landmark selection, and today’s per-pop merge), plus a `res.clone()` per candidate. This is the single worst p99 outlier source.

**Fix:** one multi-source A\*: seed the open list with every eligible `(dst, tele_cost)` (a `sources: &[(u32, f32)]` variant of `SearchParams`), recover the winning teleport from the parent chain / `path[0]`. `select_active` can score landmarks against the goal only — any subset stays admissible.
**Expected:** 10–100× on off-graph-start requests.

### 1.7 Consolidate per-node search state into one array — `medium`
`SearchContext` splits g/parent/in_open/visited_gen across four parallel arrays (`search.rs:57-64`); each relaxation touches up to 4 random cache lines. Replace with one `Vec<NodeState { g: f32, parent: u32, gen: u32 }>` (12 B — one cache line covers 5 nodes; keep the generation trick). **Expected:** 3–4× fewer node-state cache misses; ~10–20% latency at 1.1M, more at 4M where 52 MB is firmly DRAM-bound.

### 1.8 Cache h per node; read landmark rows zero-copy — `medium`
`h_active` is recomputed per *edge relaxation* (`search.rs:295`), so a grid node can pay it up to 8× before popping; each call is 16 `LeSliceF32::get`s — bounds check + `Option` + byteorder decode each (`reader.rs:188-193`) — gathering from two regions 287 MB apart. Fixes: (a) an `h_cache: Vec<f32>` guarded by the existing generation counter → h computed once per node per query; (b) slice the node’s row once and index within it; (c) with §3.3 alignment, expose true `&[f32]` and let LLVM vectorize. Cache the goal’s coords once per query (octile currently re-reads them every call — moot if octile is deleted).

### 1.9 Hot-loop micro-fixes (small, additive)
- **Heap key as one u64 compare.** `Key::cmp` does two NaN-tolerant `partial_cmp` chains per sift step (`search.rs:41-55`). f/g are non-negative finite → `to_bits()` order-preserving; pack `(f_bits << 32) | g_bits` and compare integers (the builder’s Dijkstra already does the `to_bits` trick, `landmarks.rs:66-77`). ~2–5% of search time; consider a 4-ary heap for cache behavior.
- **Fairy-ring membership without SipHash.** The boxed `per_node` closure (`engine_adapter.rs:452-464`) runs a std `HashSet::contains` on **every pop** to answer “no” 99.99% of the time (53 rings), plus a Vec alloc + re-sort merge on hits (`combine_sorted_extra`, `search.rs:178-185`). Use a sorted `Vec<u32>` + `binary_search` (6 branchless compares), and return a borrowed precomputed `&[(u32,f32)]` slice instead of allocating. 2–8 ms per long search.
- **Per-request macro eligibility bitset.** `all_neighbors` walks each macro edge’s heap-allocated `Vec<usize>` of req indices per expansion (`neighbors.rs:139-177`). With 957 macro edges, fold once per request into a bitset + effective-weight array (also folds the `quick_tele` lodestone override); per-expansion check becomes one bit test. Small win; big inlining enabler.
- **Drop the sorted-merge machinery entirely.** Relaxation is order-independent. `MergeNeighbors`, `combine_sorted_extra`, `sort_extra_edges`, and the per-node sort inside `Adjacency::build_with_data` (`neighbors.rs:53-81` — 3 temp Vecs per node, ~3.4M allocations per load) exist only to keep dst-sorted order that nothing needs. Chain the streams (walk → macro → extra) and delete the sorting. Removes ~2 branches/edge in the loop and most of the reload cost.

### 1.10 Robustness: bound the worst case (gap-analysis findings)
Currently an unreachable or ineligibility-gated goal floods the entire 1.09M-node mega-component — est. 0.5–3 s CPU today, 2–10 s at 4M, ×124 under virtual start; there is no budget, timeout, or cancellation anywhere (`grep timeout|deadline|cancel|budget` over `rust/` is empty), and dropping the handler future does **not** stop the `spawn_blocking` closure.

1. **Pop budget** (`high/small`): `max_pops` in `SearchParams`, checked at the top of the loop; return a distinct `SearchStatus::BudgetExceeded` so “gave up” ≠ “no path”. Caps p100 5–50× and makes it scale-independent.
2. **Exact reachability precheck** (`high/medium`): eligibility never gates walk edges — only macro/global/fairy. So reachability is exactly decided on a condensed graph of **490 walk components** + eligible special edges: store a u16 component id per node in the snapshot (2.2 MB today / 8 MB at 4M; union-find in the builder), BFS ≤490 vertices per request (microseconds) before `spawn_blocking`. Every impossible query drops from a multi-second flood to <100 µs rejection. All 124 globals carry requirements, so gated cross-island goals are common, not exotic.
3. **Deadline + disconnect cancellation** (`high/medium`): `cancel: AtomicBool` (or deadline) checked every ~1024 pops; set from a drop-guard in the handler and from `NAVPATH_ROUTE_TIMEOUT_MS`; add a tower `TimeoutLayer`.
4. **Concurrency semaphore** (`medium/small`): a `Semaphore(≈cores)` around `spawn_blocking` (`routes.rs:661`) so floods can’t pin 512 blocking threads × 14.6–52 MB contexts; overload fails fast with 503/Retry-After.
5. **Virtual-start interaction**: share one budget across all candidates; with the component precheck, skip candidates whose entry component can’t reach the goal; with 1.6, it all collapses into the single multi-source search anyway.

### 1.11 Bigger algorithmic upgrades (after the above)
- **Bidirectional ALT** (`high/large`): the backward infrastructure already exists — `lm_bw` is built from the reverse graph and `h_active` reads both directions from one node-major row, so the standard average potential `p(v)=(h_f(v)−h_b(v))/2` needs zero new precompute. The walk graph was **verified symmetric** (SQL over all 1.12M tiles: 0 asymmetric diagonal pairs; the 15 raw cardinal asymmetries are already deleted by the reciprocity rule), so the forward CSR can be aliased backward; only 957 macro edges + 53 fairy nodes need explicit reversal, and the backward side needs **no** global-teleport fan-out at all (globals are forward seeds per 1.2/1.6). Watch two details: price backward relaxations with the *forward* jitter identity `edge_jitter(seed, u, x)`, and add a builder-time symmetry assertion to guard future map data. Expected 1.5–3× expansion reduction on long walk-dominated routes, ~2× smaller per-query ALT page footprint. Bound the now-doubled context memory via the pool from §4.4.
- **Canonical A\* / mask-driven JPS** (`high/large`): the walk graph is a textbook uniform-cost 8-connected grid (58.7% of tiles fully open). Classic cell-blocking JPS is **not** sound here (254 measured thin-wall diagonal-only bans), so derive pruning from the stored 8-bit permission masks (canonical orderings / JPS-with-edge-constraints). Forced stops: the 921 nodes with outgoing specials (869 macro srcs + 53 fairy, 0.08% of nodes) plus the 254+15 mask anomalies. Sound because interior jumped-over nodes have only walk edges once globals are start-seeded. Prototype plane 0 (95.7% of nodes) and differential-test costs against `astar()` on replayed `result.json` queries. Expected 2–10× fewer expansions on long same-plane walks — **multiplicative** with the ALT fix (it removes tie-plateau surplus ALT can’t prune). Note the interaction with seeded jitter: serve `seed: None` traffic on the pruned path first; optionally re-derive seeded variety segment-locally later.
- **Route/response LRU cache** (`medium/medium`): requests are pure functions of `(snapshot, start, goal, mask bits, quick_tele, seed, options)` and game traffic repeats popular destinations. A 1–4k-entry LRU inside `SnapshotState` (so snapshot swap invalidates) gives near-zero latency on repeats. Guard with a hit-rate metric.

---

## 2. Snapshot build speed

Measured: 11.64 s total; ALT stage 10.6 s (91%), everything else ~1.0 s. Projected 4M serial: ALT alone 20–30 min.

### 2.1 Parallelize the ALT stage — `high / small` (the build lever)
`compute_alt_tables` (`landmarks.rs:48-60`) runs 64×2 = 128 **independent** Dijkstras sequentially (no rayon anywhere in the workspace), over `Vec<Vec<(u32,f32)>>` adjacency (2.24M heap-allocated inner Vecs, pointer-chase per relaxation), scattering results node-major with a 256-byte stride across a 287 MB table per landmark (~18 GB of write-allocate traffic total).

**Fix (all local to `landmarks.rs`):** (1) build fwd/rev **CSR** once (counting sort, flat arrays — same pattern as `neighbors.rs`); (2) `rayon par_iter` over the 128 (landmark, direction) tasks, each producing a contiguous `Vec<f32>`; (3) one cache-blocked transpose into the node-major layout at the end instead of 128 strided passes.
**Expected:** ~Ncores (8–16×) on the dominant phase + 1.5–2× per Dijkstra from CSR; total build 11.6 s → ~1.5–2 s now; a 4M build lands around 5–8 s instead of ~45+ s. Combined with 1.1’s smaller landmark count, less work again.

### 2.2 Stream the writer; stop double-buffering the file — `high / medium`
`write_snapshot` (`writer.rs:191-350`) materializes the entire payload in a `Vec<u8>` via ~170M per-element 4-byte `extend_from_slice` calls, then hashes single-threaded, then writes. This sets builder peak RSS (measured 1,525 MB; ~5 GB at 4M — OOM risk on a host showing ~7 GB free).

**Fix:** all offsets are known up front, so stream each section through a `BufWriter` wrapped in an incremental `blake3::Hasher` tee (append hash last); on little-endian, `bytemuck::cast_slice` each `&[u32]/&[f32]` section and `write_all` it in one call (per-element fallback only for BE); enable blake3’s `rayon` feature (`update_rayon`) for multi-GB hashing. Long-term: have `compute_alt_tables` write finished columns directly into a pre-sized mmap of the output region so tables never exist twice.
**Expected:** serialization+hash 2–20× faster; peak RSS at 4M drops from ~5 GB to ~2.6 GB (further with §3 size cuts).

### 2.3 SQLite ingest fixes — `medium / small`
- **Drop the SQL `ORDER BY`** (`load_sqlite.rs:40`): PK is `(x,y,plane)` so `ORDER BY plane,y,x` forces a TEMP B-TREE over all rows (measured 514 ms vs 34 ms; multi-seconds at 4M). Sort the `Vec<Tile>` in Rust on a packed u64 key — byte-identical output, ~15× faster scan. Same for the fairy query.
- **`prepare_cached` everywhere**: `fetch_db_row` (`main.rs:45-253`) and `fetch_step` (`chains.rs:114-280`) re-prepare ~6 distinct SQL strings thousands of times; lodestone even does a second prepare for the name column, and each macro edge fetches the first step’s `db_row` twice (`main.rs:347` + `main.rs:374`). Better: the teleport tables total ~1,000 rows — preload them into maps and drop per-id SQL entirely. `collect_incoming_pairs` also runs twice (once per `enumerate_*_starts`).
- **Bulk-read pragmas** (`main.rs:262-270`): current pragmas (`journal_mode/synchronous`) are write-side no-ops on a read-only connection. Add `mmap_size`, `cache_size`, `temp_store=MEMORY`.
- **DB size hygiene**: `idx_tiles_walkable ON tiles(x,y,plane)` is an exact duplicate of the PK autoindex (verified via `PRAGMA index_info`) — 16.1 MB of the 58 MB file; the autoindex is another 17.7 MB. `DROP INDEX` + declare `tiles` `WITHOUT ROWID` → DB shrinks ~58 → ~25 MB with zero loader changes.
- **`tiles.bin` is written one byte per `write()` syscall** (`main.rs:554-562`): 1.12M syscalls for a 1.1 MB file. Buffer it. Seconds → <10 ms.
- **Dead code:** `load_localized_teleports` (`load_sqlite.rs:62-101`) has no callers; delete (and the no-op cast chain at `load_sqlite.rs:51`).

### 2.4 Kill SipHash tuple keys in the build hot loop — `medium / small`
`node_id_of: HashMap<(i32,i32,i32), u32>` (`main.rs:291-294`) is probed up to 8×/tile by `compile_walk_edges` (`graph.rs:56`) — ~9M SipHash probes today, ~32M at 4M — plus per-endpoint probes in chains/fairy. Coordinates pack losslessly into u64 (x 995–5731, y 84–10261, plane 0–3). Options in ascending win: `FxHashMap<u64,u32>`; sorted `Vec<u64>` + binary search (tiles are already sorted by the same key); dense per-(plane,y) row index → O(1) arithmetic lookups. 2–5× on edge compilation; also pre-size the walk vectors (`graph.rs:9-11` start empty and realloc ~180 MB while growing; reserve `tiles.len()*7`).

### 2.5 Region-blob tile format (4M-scale ingest) — `high / large`
Each tile carries ~1 byte of true payload (walk_mask ≤ 255; coords derivable) but costs ~52 B across three B-trees, decoded row-at-a-time. Add a producer table `tiles_regions(region_id, plane, base_x, base_y, masks BLOB(4096)) WITHOUT ROWID` (one row per 64×64 region; ~2,800 rows at 4M): ingest becomes region-row reads + memcpy, sub-100 ms at 4M, DB ~5–12 MB now / ~20–40 MB at 4M (keeps the DB committable to git). Keep the old path as fallback when the table is absent.

### 2.6 Incremental rebuild note
A cached-ALT scheme keyed on the walk-graph hash was considered and **refuted**: ALT tables depend on macro (teleport) edges too (`landmarks.rs:31-39`), so teleport-only DB edits still invalidate them. If incremental builds are wanted later, key the cache on the hash of (walk edges + macro edges + landmark set) — or rely on §2.1 making full rebuilds cheap enough not to care.

---

## 3. Snapshot format v8 proposal (size, load time, and read-path speed together)

Bundle these into **one** version bump (`manifest.rs:6`, `SNAPSHOT_VERSION 7 → 8`); each also improves the hot loop. Current file is 609 B/node; the proposal lands around **~130–260 B/node**.

### 3.1 Quantize ALT tables to u16 and interleave fw/bw — `high`
574 MB → f32 precision is pointless for ms-scale costs (min edge 300). Store u16 in fixed quanta (e.g. 64 ms/unit → 70 min range, or 300 ms tile units; `0xFFFF` = unreachable — this also fixes the INFINITY poisoning of §1.4). Round so the derived bound stays a lower bound (floor stores, subtract one quantum after the max, clamp ≥ 0) → admissibility preserved exactly. **Interleave** `[node][landmark][fw,bw]` so one heuristic call reads one contiguous row instead of two regions 287 MB apart.
**Effect:** 2× from u16, ~2.7–4× from fewer landmarks (§1.1): 574 MB → ~72–143 MB today; at 4M, 2.05 GB → ~256–512 MB. Doubles heuristic cache-line density.

### 3.2 Make the walk graph CSR-native — or store masks only — `high`
Today the snapshot stores unsorted COO triples; every startup **and** `/admin/reload` copies 3×7.5M values out of the mmap, rebuilds CSR with per-node sorts (~3.4M allocations; ~12M at 4M), and duplicates ~70 MB (~250 MB at 4M) on the heap next to the mmap (`engine_adapter.rs:204-215`, `neighbors.rs:30-84`). Two options:
- **Option A — CSR on disk:** `walk_offsets: u32[n+1]` + dst (+ w or a diagonal bit) pre-sorted by the builder; `NeighborProvider` wraps mmap slices zero-copy. Startup/reload → near-instant. Lower risk.
- **Option B — chunked effective-mask grid (preferred long-term):** store one byte per tile — the *effective* 8-direction mask after reciprocity/diagonal filtering (bake `graph.rs:47-102` rules in at build time) — in 64×64 chunk pages with a per-plane chunk directory (772 occupied chunks = 3.16 MB today, ~11 MB at 4M vs 90→325 MB). Neighbor generation becomes one byte load + bit tests; weights derive from direction (300/424.26 — `walk_w` today spends 30 MB of f32 encoding one bit that’s implied by the direction anyway); this is also the exact data structure canonical-A\*/JPS jump scans need (§1.11), and with chunk-major node numbering it retires the coord HashMap too.
- Either way, **drop `walk_w`** (derivable) and keep the 957 macro edges in their current form.

### 3.3 Alignment + zero-copy typed views — `medium`
`off_req_tags = off_macro_meta_blob + blob.len()` (`writer.rs:110`) with a 892,041-byte blob leaves **every hot section misaligned** (measured `off_lm_fw = 109,256,053 ≡ 1 mod 4`) — every f32 read in the hottest tables is an unaligned load and safe `&[f32]` casting is impossible, which is why all reads go through per-element bounds-checked byteorder decoding. Pad every section start to 64 B; after `validate_layout`, expose real `&[u32]/&[i32]/&[f32]` via `bytemuck::cast_slice` (LE targets; keep LeSlice as fallback). Unlocks §1.8 and SIMD `select_active`. 5–15% of search time.

### 3.4 Node table cleanup — `medium/small`
- `nodes_ids` is the identity sequence with **zero consumers** — 4.5 MB dead (16 MB at 4M). Drop it.
- `x/y/plane` are three separate sections (3 scattered reads per coord lookup); ranges fit 14+15+2 bits → one packed u32 array. −13.5 MB now / −48 MB at 4M.
- Emit a **coord-index section** (sorted packed-key → id array, or implicit via chunk-major numbering) so the service stops rebuilding a SipHash `HashMap<(i32,i32,i32),u32>` (~142 MB heap at 4M, seconds of build, rebuilt on every reload — `lib.rs:62-75`) to serve two lookups per request; binary search over the mmap is ~22 probes.
- Add the **walk-component id section** from §1.10.2.

### 3.5 mmap hygiene — `medium/small`
`Snapshot::open` does a bare `Mmap::map` (`reader.rs:22-31`). Use `memmap2`’s advise API: `populate()`/`Advice::WillNeed` for the ALT sections when RAM allows, else `Advice::Random` to stop useless readahead. Note the blake3 tail is written but **never verified at open** — today that keeps load O(1) (good); if integrity checking is ever wanted, verify in the background after swap, not on the load path.

### 3.6 (Uncertain, benchmark first) Hilbert/Morton node ordering
Node ids follow `(plane,y,x)` row order, so a K×K search region touches K disjoint index runs spread across the ALT table (~0.5–2 MB apart per y-step). Renumbering along a per-plane Hilbert curve at build time (builder-only change — everything downstream is index-based) would cluster a regional query’s working set by orders of magnitude fewer pages at 4M. Flagged uncertain because the win depends on access patterns; cheap to A/B once the benchmark harness (§6.4) exists.

### Projected sizes

| | Today (v7) | v8 (u16 ALT ×24 lm + CSR walk + cleanups) | v8 + mask-grid walk |
|---|---|---|---|
| 1.12M nodes | 684 MB | ~250–290 MB | ~160–200 MB |
| 4M nodes | ~2.43 GB | ~0.9–1.05 GB | ~0.55–0.7 GB |

---

## 4. Service layer

### 4.1 Stop re-parsing the 113 KB global JSON per route — `high / small`
After **every** found route — even with neither actions nor geometry requested — `routes.rs:707-757` re-parses the (0,0) macro edge’s 113,093-byte JSON (124 teleports), builds two HashMaps, and clones subtrees. `build_neighbor_provider` already parses this exact JSON at load and throws the parsed values away (`engine_adapter.rs:137-172`). Cache `meta: Arc<serde_json::Value>` (+ kind string) on `GlobalTeleport` at load; filter by mask at request time; skip the block entirely when not needed. Saves ~1–3 ms p50 on every found route.

### 4.2 Build eligibility state once; fix the coercion divergence — `medium / small` (correctness!)
The mask + req-id map are built in `routes.rs:619-650` and then **again** inside `spawn_blocking` (`engine_adapter.rs:342-363`) — and the two copies coerce differently: routes.rs maps JSON `true`→`Num(1)` and parses numeric strings; the adapter ignores bools and truncates via `as_f64`. A client sending `true` gets the edge annotated but **not searched** (or vice versa). Build the mask + `quick_tele` once in routes.rs with one shared helper, pass `EligibilityMask` in; move `req_id→tag_idx` into `SnapshotState` (snapshot-constant, currently derived per request and at load).

### 4.3 Response building: typed, off the runtime threads, no duplicate payloads — `medium`
All post-search work — per-edge action construction, **two** JSON parses per on-path macro edge (`macro_edge_allowed_by_profile` at `routes.rs:787`, again at 820-824), surge/dive optimization that `clone()`s every action (`routes.rs:237,453`), final serialization — runs on the tokio reactor threads after `spawn_blocking` returns; for 1000+-step paths that’s thousands of `serde_json::Value` allocations blocking the event loop, and the response duplicates the route up to 3× (`path` always serialized even with `only_actions`; `res.path.clone()` at 983). Move construction into the blocking closure; use `#[derive(Serialize)]` typed actions and `geometry: Vec<[i32;3]>`; reuse the first parse; `mem::take` the path and omit it under `only_actions`. 1–5 ms less reactor blocking per long-path request; 30–50% smaller payloads.

### 4.4 Bound search concurrency and pool the contexts — `high` at 4M
`spawn_blocking` per request (`routes.rs:661`) + `thread_local!` `SearchContext` (`engine_adapter.rs:14-16`) on a pool that grows to 512 threads and reaps idle threads after ~10 s: bursts materialize hundreds of contexts (14.6 MB each today, 52 MB at 4M → tens of GB worst case) and low-QPS traffic repeatedly re-pays a multi-MB alloc+memset on cold threads. Use a dedicated fixed-size worker pool (≈ physical cores) each owning a persistent context — or a bounded `ArrayQueue<SearchContext>` + semaphore. Caps memory at cores×52 MB and removes cold-thread p99 spikes. (Doubles per §1.11 bidirectional — size the pool accordingly.)

### 4.5 Reload path — `medium`
`/admin/reload` (`routes.rs:1007-1032`) runs `Snapshot::open` + full CSR rebuild + coord-index rebuild inline on an async runtime thread — seconds of reactor stall today, tens of seconds at 4M, with both old and new states resident. §3.2/§3.4 make reload near-instant; until then, wrap the body in `spawn_blocking`.

### 4.6 Small
- `TCP_NODELAY` is never set on accepted sockets (`main.rs:50-51`); 100 KB+ JSON responses over many segments can eat delayed-ACK stalls on some client stacks. Two lines in an accept loop; verify clients use keep-alive.
- `run_route` / `run_route_with_requirements` wrappers re-collect `req_tags` per call; fold into the §4.2 cleanup.

---

## 5. The 4M-tile question: does flat A\* survive?

**What breaks at 4M with no changes:** ALT tables 2.05 GB (page-fault storms under memory pressure — host shows ~7 GB free); near-Dijkstra searches pop ~4M nodes worst case with 124-way merges per pop; per-thread contexts 52 MB × up to 512 threads; startup/reload rebuilds (CSR + coord map + 4M SipHash inserts) take tens of seconds; builder peak RSS ~5 GB and ALT build 20–30 min serial. Counters are safe: u32 node/edge counts and u64 offsets comfortably hold 4M nodes / ~27M edges — the *format* survives; the architecture as-is does not.

**Two viable paths:**

**Path A — incremental (Phases 1–3 below), no rearchitecture.** Fixed landmarks + globals-at-start + stale-skip + budget/precheck + u16 ALT + CSR/mask-native snapshot already change the picture qualitatively: typical searches stop scaling with tile count (they scale with route length × frontier width), the snapshot lands ≲1 GB, and worst cases are budget-capped rather than unbounded. Add bidirectional ALT and canonical/JPS (§1.11) and expansion counts drop another ~3–10× on the dominant long-walk queries. **This is very likely sufficient for 4M if p99 targets are tens of ms.**

**Path B — HPA\*-style two-level routing (the scalability agent’s verdict for worst-case guarantees at 4M+).** `RegionID` already partitions the map (≤4096 tiles/region; 705 regions now, ~2500 at 4M). Plain CH is ruled out by per-request eligibility, but HPA fits cleanly because abstract shortcuts span only unconditional walk edges:
1. Builder: per region, find border-entrance tiles (dedupe per contiguous border segment); all-pairs Dijkstra among entrances inside each region (≤4096 tiles, trivially parallel).
2. Promote macro srcs/dsts, global dsts, and fairy nodes into the overlay graph, keeping their requirement tags for per-request filtering.
3. Query: local Dijkstra in start/goal regions to entrances → A\* on the ~20–50k-node overlay (a tiny ALT table here is cheap) → refine tile geometry only for regions on the chosen abstract path.
4. Keep flat A\* behind a flag during rollout; snapshot v9 adds entrance/overlay sections, tile sections unchanged.
Effect: worst-case requests go from seconds to low ms at 4M and the 2 GB ALT tables are replaced by MB-scale entrance tables (snapshot ~0.5 GB).

**Recommendation:** execute Path A first — every item in it is needed regardless and is 10–100× cheaper to build; instrument with the §6.4 benchmark corpus; commit to Path B only if measured 4M worst-case latency still misses targets.

---

## 6. Toolchain & infrastructure

1. **`target-cpu`** — no `.cargo/config.toml` exists; binaries target baseline x86-64 (SSE2) on a Zen 4 (AVX-512) host. Add `-C target-cpu=native` (or `x86-64-v3` if shipped elsewhere). 5–15% on vectorizable loops, free.
2. **mimalloc** — glibc malloc serves the allocation-heavy builder ALT/meta phases and service reload churn. Two lines per binary (`#[global_allocator]`). 10–25% on builder allocation phases.
3. **PGO / BOLT** — ideal workload (branch-heavy A\*, deterministic replay corpora already in-repo: `result.json`, `results_parsed.json`); composes with the existing fat-LTO/CGU=1 profile via `cargo-pgo`. 5–15%. BOLT needs `strip = "none"` in the profile used.
4. **Benchmark + profiling harness (do this first)** — nothing is measurable today: no benches, no criterion, and `strip = true` breaks flamegraphs. Add criterion benches in `navpath-core` (fixed ~20 start/goal corpus over the real snapshot via env var; `h_active` microbench; provider-build bench) and a `[profile.profiling] inherits = "release", debug = "line-tables-only", strip = "none"` for `cargo flamegraph`/samply. Every other item in this doc should land with before/after numbers from this harness.
5. **Hygiene** — `navpath-service` declares `rusqlite` (links all of SQLite for nothing) and `itertools` with zero uses: remove both (helps the slow fat-LTO link). Delete the silently-ignored `[profile.release]` in `rust/navpath-builder/Cargo.toml:16-19` (workspace member profiles don’t apply; root wins). Consider `cargo-machete` in CI.

---

## 7. Recommended execution order

**Phase 0 — measure (½ day):** §6.4 harness + profiling profile. Baseline p50/p99 on a fixed corpus (short/long/cross-plane/teleport-heavy/unreachable).

**Phase 1 — rebuild-only + tiny code fixes (1–2 days, no format change):**
landmark farthest-point selection @ 16–24 (§1.1) → globals-at-start (§1.2) → stale-pop skip + delete `in_open` (§1.3) → drop octile / add teleport cap (§1.4) → high-g tie-break (§1.5) → pop budget (§1.10.1) → rayon ALT + CSR + transpose (§2.1) → SQL quick fixes (§2.3) → target-cpu + mimalloc (§6.1–2).
*Expected compound: roughly an order of magnitude on long-route latency; build 11.6 s → ~2 s; snapshot shrinks ~2.7× from the landmark count alone.*

**Phase 2 — service + engine internals (2–4 days):**
multi-source virtual start (§1.6) → AoS search state (§1.7) → h-cache (§1.8) → global-JSON caching (§4.1) → eligibility-once + coercion fix (§4.2) → typed responses in the blocking closure (§4.3) → bounded search pool (§4.4) → fairy binary-search + macro bitset + merge removal (§1.9) → deadline/cancellation + semaphore (§1.10.3–4).

**Phase 3 — snapshot v8 (3–5 days, one version bump):**
u16 interleaved ALT (§3.1) → CSR-or-mask walk graph, drop `walk_w` (§3.2) → 64 B section alignment + zero-copy views (§3.3) → packed coords, drop `nodes_ids`, coord-index + component-id sections (§3.4, §1.10.2) → streaming writer (§2.2) → madvise (§3.5).
*Expected: snapshot 684 → ~200–290 MB (4M: ~0.6–1 GB); startup/reload near-instant; impossible queries rejected in µs.*

**Phase 4 — algorithmic upgrades, benchmark-gated:**
bidirectional ALT (§1.11) → canonical A\*/JPS on the mask grid (§1.11) → LRU cache (§1.11) → Hilbert ordering A/B (§3.6) → PGO/BOLT (§6.3) → region-blob DB format (§2.5).

**Phase 5 — only if 4M worst-case targets still missed:** HPA\* overlay (§5 Path B).

---

## Implementation log

### Phases 0–1 — DONE (2026-07-05)

Everything in Phase 0 and Phase 1 is implemented and measured (criterion corpus in `rust/navpath-core/benches/astar.rs`, diagnostics in `examples/probe.rs`; snapshots rebuilt with `--landmarks 24`):

| Metric | Baseline | After Phase 1 |
|---|---|---|
| Short routes (~150 tiles) | 109–492 ms | **8.8–21 µs** (~12,000–23,500×) |
| Teleport-entry long routes | 641–664 ms | **230 µs–4.6 ms** (140–2,800×) |
| Teleport-entry medium routes | 62–120 ms | 9–17.6 ms (6–7×) |
| Statically-unreachable query | 8.8 ms full flood | **~100 ns, 0 pops** |
| Unbudgeted flood (no goal-side landmark) | 946 ms | 318 ms (and budget-capped to ≤500k pops in the service) |
| ALT build stage | 10.6 s | 2.06 s |
| Snapshot | 684 MB | 325 MB |
| Tile DB | 58 MB | 39.4 MB |
| `select_active` | 214 ns | 97 ns |

Deviations from the plan, discovered by measurement (§1.4's cap evolved):
1. **The injected-edge heuristic cap is NOT implemented as a runtime cap.** Capping h by `min(w_t + h(dst_t))` over global teleports flattens the heuristic to a near-constant (globals are start-seeded only, so the cap was pure loss — measured 2× slower mediums). Instead the **builder bakes the fairy-ring clique and quick-tele-floored lodestone weights into the ALT graph** (`main.rs`, `landmarks.rs`), restoring the invariant "tables cover a superset of mid-search edges" with zero hot-loop cost. Snapshots older than this change are mildly inadmissible around fairy rings — rebuild.
2. **`select_active` filters landmarks by finite GOAL entries only** (`heuristics.rs`). A node's own INF entry then yields h=+INF, which is *proof* the node cannot reach the goal (`d(u,L) <= d(u,goal)+d(goal,L)`).
3. **Nodes with h=+INF are never pushed** (`search.rs`). This (a) rejects statically-impossible queries in ~0 pops whenever the goal's component has a landmark — most of §1.10.2's reachability precheck for free — and (b) avoids an f=INF heap plateau where tie-breaking degrades into DFS-like mass reopenings (observed: 534M pops before this guard).
4. `SearchResult` gained a `pops` counter (diagnostics/budget tuning) alongside `status`.

Also landed: duplicate SQLite index dropped (58→39.4 MB; producer should stop creating `idx_tiles_walkable` and consider `WITHOUT ROWID` for `tiles`), `.cargo/config.toml` with `target-cpu=native`, mimalloc in both binaries, `[profile.profiling]` + `[profile.bench]`, dead `load_localized_teleports` removed, service `rusqlite`/`itertools` deps removed, builder's ignored `[profile.release]` removed.

Still open from the writer findings: the snapshot writer is now the dominant build phase (~8.5 s of ~11 s) — lands with §2.2/§3 in Phase 3.

### Phase 2 — DONE except §4.3/§1.9c (2026-07-05)

Landed (all workspace tests green; bench deltas vs Phase 1):
- **Multi-source virtual start** (§1.6): `EngineView::astar_multi` seeds every eligible teleport dst at `g = cost`, winning entry = `path[0]`; the up-to-124-searches loop in `run_route_with_requirements_virtual_start` is gone.
- **AoS search state + h-cache** (§1.7/§1.8): one 16-byte `NodeState { g, h, parent, gen }` per node; the ALT heuristic is computed at most once per node per query (NAN-marked lazily). Long benches −27–36%, shorts −20%, mediums −10–25% on top of Phase 1.
- **Sorted-merge machinery deleted** (§1.9d): `MergeNeighbors`, per-pop dst-order merging, and the per-node sort in `Adjacency::build_with_data` (~3.4M allocations per load) are gone; streams are chained. Determinism now comes from builder emission order.
- **Global-JSON caching** (§4.1): `GlobalTeleport.meta: Arc<Value>` parsed once at load; `/route` no longer re-parses the 113KB blob, and the annotation block is skipped entirely unless actions/geometry were requested.
- **Eligibility once + coercion fix** (§4.2): the mask (with bool/numeric-string coercion) and `quick_tele` are built once in `routes.rs` and passed through; the adapter's divergent duplicate is deleted. (Deviation: the tiny req-id→tag map stays per-request rather than in `SnapshotState` — 129 inserts, not worth the constructor churn.)
- **Deadline + disconnect cancellation** (§1.10.3): `SearchParams.cancel: &AtomicBool` checked every 1024 pops (`SearchStatus::Cancelled`, response reason `"cancelled"`); routes set it from a drop-guard (client disconnect) and `NAVPATH_ROUTE_TIMEOUT_MS` (default 10s → 504).
- **Concurrency semaphore** (§1.10.4/§4.4): `AppState.search_permits` from `NAVPATH_MAX_CONCURRENT_SEARCHES` (default = cores); overload returns 503 instead of piling blocking threads. (Contexts remain thread-local but concurrency-bounded; a checkout pool can follow.)
- **Streaming snapshot writer** (§2.2, pulled forward from Phase 3 — no format change): sections stream through a BufWriter + incremental blake3 tee, zero-copy on LE. Builder peak RSS **1,525 → 743 MB**; encode CPU eliminated (remaining write phase is disk-bound).
- **Atomic snapshot writes** (new finding): the writer previously truncated the output in place — a process (service after `/admin/reload`, or a running bench) still mmap-ing the old file SIGBUSes on its next page touch. Now writes `<path>.tmp` + `rename`. This was observed live (bench crashed with SIGBUS during a concurrent rebuild).
- `SearchResult.pops` diagnostics; probe example `examples/probe.rs`.

~~Remaining from Phase 2: §4.3 typed response building inside the blocking closure and the §1.9c macro-eligibility bitset.~~ Both landed after Phase 2 (see below).

### Phase 2 leftovers — DONE (2026-07-05)
- **§1.9c `MacroFilter`**: per-request allow-bitset + effective weights folded once (also folds the quick-tele lodestone override); `SearchParams` carries `macro_filter` instead of `mask`+`quick_tele`; the relax loop does one bool index per macro edge.
- **§4.3**: all action/geometry construction moved into the `spawn_blocking` closure (`build_route_payload`) — the reactor never builds JSON for 1000-step paths; on-path macro metadata parsed once (`macro_edge_meta_if_allowed`); `res.path` no longer cloned; `only_actions` responses omit the duplicate `path` array (verified live: `path` absent, actions intact).

### Phase 3 — snapshot v8 — DONE (2026-07-06)

One format bump (`SNAPSHOT_VERSION = 8`), everything from §3 except Hilbert ordering (§3.6, still benchmark-gated future work):
- **u16 quantized interleaved ALT table** (`lm_tab`, `[node][landmark][fw,bw]`, **64 ms** quanta, floor-stored, one quantum subtracted on read; `0xFFFF` unreachable / `0xFFFE` saturated). Two saturation rules are load-bearing: a saturated forward entry is never used (understating `d(L,u)` overstates the bound), and **landmarks with a saturated goal entry are dropped for the query** — a 16 ms quantum experiment (range 1.05M ms < real cross-map distances) surfaced this as a live suboptimal-route bug (44909 vs the true 41649 on the reference route) before the filter existed. 64 ms gives a 4.19M ms range (~14k walk tiles), so saturation is a safety net, not a working state; the 16 ms trial also proved the plateau, not quantization slack, drives the short-route pop count (2273 vs 2352 pops).
- **CSR-native walk graph**: `walk_offsets + walk_dst + diagonal bitmap`; `walk_w`/`walk_src` gone — weights derived (300 / 300·√2, bit-exact with the old stored values). The engine's `WalkGraph::Csr` borrows the mmap zero-copy; only the 957 macro edges are heap-built at load.
- **Packed coords** (`plane<<30|y<<15|x`, one u32/node) replacing `nodes_ids/x/y/plane`; since node ids follow (plane,y,x) order, **coordinate→id lookup is a binary search over the mmap** — `build_coord_index` and the 1.12M-entry HashMap are deleted.
- **Walk-component ids** (u16/node, 490 components — matching the audit's count) stored for future exact reachability prechecks (the eligibility-aware condensed BFS of §1.10.2 remains unimplemented; h=∞ pruning already covers every goal with a landmark in its component).
- **64-byte section alignment + zero-copy typed slices** (`LeSlice*` wrappers deleted; per-element bounds-checked byteorder decoding gone from all hot paths).
- **madvise**: `Advice::Random` on open (+ `NAVPATH_MMAP_POPULATE=1` for pre-faulting).

**Measured:** snapshot **684 MB (v7 baseline) → 325 MB (24 landmarks) → 150.9 MB (v8)**; at 4M tiles projected ~540 MB. Service **startup 211 ms**, **`/admin/reload` 20 ms** (was seconds — no CSR/coord-index rebuild). Builder 6.6 s wall / 731 MB peak RSS. Flood −22%, `select_active` −36%. Route costs bit-identical (verified live: same 41648.527 on the reference route).

**Known trade-off (documented, accepted):** quantized ALT loses exact-tie discrimination on the equal-cost plateau — nodes on alternative optimal paths drop ε below the goal's f and pop first. Short routes: 138 pops/12 µs (Phase 2, f32 tables) → ~2.3k pops/~150–370 µs (v8); longs ~0.5–1 ms; mediums/flood unchanged or better. Absolute latencies remain far under target; if the last 20× on short routes ever matters, options are an ε-weighted tie-break (bounded 0.01% suboptimality) or an optional f32 residual side-table.

**Remaining (Phase 4, benchmark-gated):** bidirectional ALT, canonical A*/mask-driven JPS, LRU response cache, Hilbert node ordering A/B, PGO/BOLT, region-blob DB ingest format, HPA* overlay only if 4M worst-case targets are missed.

### Phase 4 — DONE except deferred items (2026-07-06)

- **LRU route cache** (§1.11): per-snapshot (swap-invalidated) LRU keyed by (virtual-start, sid, gid, mask bits, quick_tele, seed); stores the raw `SearchResult` + winning entry so one entry serves every options combination (payload rebuilt per request). `NAVPATH_ROUTE_CACHE` entries, default 2048, 0 disables. Only Found/NotFound outcomes are cached — budget/cancel truncations never stick. Hits skip the search *and* the concurrency permit.
- **Region-blob ingest** (§2.5): `tiles_regions` table (one row per 64×64 region: 512-byte presence bitmap + 4096 mask bytes — the bitmap is required because `walk_mask=0` is legal on 23 teleport-only tiles) with `migrate_tiles_regions.py` producing it (772 rows for 1.12M tiles) and a builder fast path that falls back to row-per-tile. **Verified byte-identical snapshots** across region-path ×2 and legacy-path builds (also proving the builder fully deterministic). DB producer should emit `tiles_regions` natively going forward.
- **Bidirectional ALT** (§1.11): `EngineView::astar_bidir` in the **MM formulation** (`pr = max(g+h, 2g)`, stop at `mu <= min(prmin_f, prmin_b)`) — proven correct with admissible-only bounds, so ALT quantization inconsistency cannot break exactness. Backward bounds (`select_active_rev`/`h_active_rev`) are anchored on the full forward origin set (start + seeded teleports) via per-landmark anchor aggregates with the same saturation side-rules as forward. Backward graph: forward walk CSR reused (the **builder now asserts walk symmetry**, guarding this precondition — a directed test fixture demonstrated exactly how asymmetry breaks the termination proof), reversed macro provider built at load, fairy predecessors of ring x = all other rings at cost(x), globals forward-only. **Differential: 0 cost mismatches over 500+ random real-snapshot pairs; 1.47× fewer pops than unidirectional** (the earlier plain-f termination variant measured 0.78× — slower — and was replaced). Default on; `NAVPATH_BIDIR=0` falls back to unidirectional.
- **PGO: measured, not adopted.** Full cycle run (llvm-tools `profile-generate` → 300-pair engine replay → `llvm-profdata merge` → `profile-use` rebuild): 500-pair differential workload 10.42 s PGO vs 10.28 s plain — no win. The hot path is memory-bound (ALT row gathers, heap traffic), not branch-bound, so profile-guided layout has nothing to optimize. Recipe (compose with `-C target-cpu=native`, since `RUSTFLAGS` overrides `.cargo/config.toml`): `RUSTFLAGS="-C target-cpu=native -C profile-generate=DIR" cargo build --release …`, run workload, merge, rebuild with `-C profile-use`.
- **Deferred, with reasons:** *Hilbert ordering* — v8's coordinate lookup depends on (plane,y,x) node order (mmap binary search); reordering needs a permutation section and only pays under memory pressure at 4M — revisit with real 4M data. *Canonical A\*/mask-driven JPS* — multi-session project; its main wins (plateau collapse, long-walk pruning) partially overlap bidir + cache; still the right next lever if short-route µs latency becomes a goal. *BOLT* — pointless given the PGO result (same memory-bound bottleneck).

Phase 4 live verification: repeat request 13 ms (first, cold pages) → **0 ms on cache hits**; `NAVPATH_BIDIR=0` kill-switch returns identical costs; all 11 test suites green. Note: the deployed `graph.snapshot` is operator-built with `--landmarks 64` (330 MB) — v8 supports any count; 24 gives 151 MB at slightly weaker bounds, 64 gives stronger pruning. Both are valid operating points.

### Production incident 2026-07-06 01:15 — budget_exceeded on a reachable route (RESOLVED)

A real request ((2887,3535,0)→(3563,3408,0), quick-tele profile with most item teleports gated) returned `found=false, reason=budget_exceeded`. Diagnosis chain, each step verified live:
1. The pair, snapshot (operator 64-landmark build, confirmed Q64 tables), and unseeded request all succeeded — ~300–480k pops, *just* under the 500k budget.
2. The client's wrapper (`python3 -m navpath`) always sends a **`seed`**. Jitter separates otherwise-equal f values, which disables the exact-tie high-g collapse of the quantization plateau — measured: every seed pushed this query over 500k pops while unseeded stayed under. Bidir on/off made no difference (not implicated).
3. **Fixes:** default `NAVPATH_MAX_POPS` raised 500k → **1.5M** (hard legit queries measured up to ~600k pops; floods remain capped at a few hundred ms, with the deadline + semaphore bounding aggregate damage). Verified: seeds 1/2/12345 all find the route in 140–200 ms; production `:8080` restarted on the fixed binary and re-verified with the exact failing request.
4. **Hardening from the same investigation:** `ALT_QUANTUM_MS` is now **stamped into the snapshot header** and the reader uses the stored value (0 in legacy files decodes as 64 ms) — a binary/snapshot quantum mismatch previously mis-scaled every heuristic silently.
5. Client-side note (their repo): `ResultsParser.kt` throws on any response without an `actions` array — but `found=false` is a legitimate outcome (genuinely unreachable goals). Their parser should handle `{"found": false, "reason": ...}` gracefully.

## Appendix A — cross-cutting correctness issues found during the audit

These affect result *quality* today and should ride along with the perf work:
1. Heuristic inadmissibility (three sources — §1.4): suboptimal paths biased away from teleports; INFINITY/NaN degradation on unreachable-landmark pairs.
2. Eligibility coercion divergence between search and annotation (§4.2): `true`-valued requirements are honored in one place and ignored in the other.
3. `edge_jitter` asymmetry matters if bidirectional lands (§1.11): backward relaxations must hash the forward edge identity.
4. The blake3 integrity tail is never checked at open (§3.5) — fine for latency, but worth knowing it provides no protection today.

## Appendix B — refuted / uncertain
- **Refuted:** “cache ALT tables keyed on tile-graph hash for teleport-only edits” — ALT depends on macro edges too (`landmarks.rs:31-39`); key on the full (walk+macro+landmarks) hash if ever needed.
- **Uncertain (benchmark before committing):** Hilbert node ordering (§3.6); the exact octile replacement policy (§1.4 — delete vs capped-max: measure after landmarks are fixed, deletion is the default recommendation).
