# Path-Calculation Latency — Repo Analysis & Optimization Plan (v3)

Date: 2026-08-06. Supersedes `docs/optimization_roadmap_v2.md` (deleted from the working
tree; recoverable via `git show dc78e3c:docs/optimization_roadmap_v2.md`). That roadmap's
implementation log is the authoritative record of what already landed and what was
measured; this doc summarizes it, re-verifies the live state of the tree, and lays out
what is still worth doing — including the "remove the jitter/seed" suggestion, which
**checks out** (see §3, the headline item).

> The `.spec-workflow/specs/navpath-performance-optimizations/` spec describes an
> architecture that no longer exists (f32 native tables, bucket queue, bitvec masks —
> all superseded or measured-and-rejected). Treat it as historical; do not implement
> from it.

---

## 1. System analysis — how a path gets computed

Three crates:

- **`navpath-core`** — the engine. A memory-mapped v8 snapshot (walk grid as zero-copy
  CSR with a diagonal bitmap; ~1k macro edges; packed coords; walk-component ids; an
  interleaved u16 ALT landmark table quantized at 64 ms) feeds an A\* engine
  (`engine/search.rs`) with:
  - **ALT (landmark) heuristic**, full-width over all landmarks per call, branchless
    u16 SIMD (`h_active`, 6.5 ns/node autovectorized; an explicit AVX-512 path is
    in-flight in the working tree, measured 8.7 → 3.6 ns warm).
  - **Bidirectional MM** search as the production default, with a weak-backward
    demotion policy (`NAVPATH_BIDIR_MIN_HB_RATIO=0.5`) that falls back to
    unidirectional when the backward bound is weak.
  - **Canonical strict-domination pruning** (Phase E Stage 2a) — a per-(node,
    incoming-direction) successor table derived from the CSR at load; cost-exact;
    **engages only for unseeded searches**.
  - Packed 16-byte heap keys, incumbent/mu push pruning, has-macro bitmap, software
    prefetch, `alloc_zeroed` contexts, generation-stamped `NodeState` — the Phase B–E
    hot-loop program, all landed and measured.
- **`navpath-builder`** — SQLite tile DB → snapshot. Farthest-point landmark selection,
  parallel backward columns, blocked transpose emitting interleaved u16 directly.
  Full 64-landmark build ≈ 4.9 s.
- **`navpath-service`** — axum/tokio HTTP. Per request: eligibility mask → route-cache
  lookup → **component-graph reachability precheck** (kills unreachable/gated floods in
  µs) → profile-artifact cache → semaphore permit → `spawn_blocking` search via a
  bounded `ContextPool` → typed payload build → LRU route cache. Budget-exceeded
  searches climb a retry ladder (same-seed at 4× budget, then unseeded, marked
  `degraded: "seed_dropped"`).

### Current performance profile (blessed criterion medians, 64-landmark snapshot)

| Class | Median |
|---|---|
| Unseeded uni short / medium / long | 39–112 µs / 82–195 µs / 0.77–1.72 ms |
| Bidir (production engine) short / medium / long | 103–340 µs / 122–170 µs / 2.0–3.5 ms |
| **Seeded + budgeted** (production request shape) uni long | **6.7–17.7 ms** |
| **Seeded + budgeted** bidir long | **4.2–9.6 ms** |
| **Gated profile (lodestone-only quick-tele), reachable** | **190–220 ms** (budget-capped flood) |
| Incident pair, bidir gated seeded | 24.9 ms |
| Cross-plane flood (not-found worst case) | 244 ms |
| `h_active` | 6.5 ns/node |

Where the remaining time actually is:

1. **Seeded traffic is 3–10× slower than unseeded at every bucket** — and production
   traffic is ~always seeded. The dominant component is the **route-cache miss** (seed
   is in the cache key → ~0% hit rate), not the search itself. See §3.
2. **The gated-profile tail (190–220 ms)** — reachable-but-heavily-gated goals flood to
   the 1.5M-pop budget (then retry at 4×). The component precheck only removes
   *unreachable* goals.
3. **Pop count, not per-pop cost, is the engine lever left.** Heuristic compute is
   6.5 ns/node; full-width ALT already collapsed the quantization plateau (which is why
   bucketed tie-break and Stage-2b tie-pruning both lost most of their premise).
4. **Measurement debt (new since the last bless):** the deployed `graph.snapshot` is
   now **1,133,793 nodes / 32 landmarks / 189 MB**, but the blessed perf baselines and
   every number above came from the **64-landmark, 330 MB** snapshot (and the
   64-landmark configuration is what the full-width-ALT pop reductions were measured
   on). Nothing can be gated until baselines are re-blessed — and the 32-landmark
   downgrade itself may be a live pop regression (§5.1).

---

## 2. Already done — do not re-propose

Phases A–E of roadmap v2 landed and were verified bit-exact via the replay corpus:
correctness fixes (truncation status, virtual-start fairy, exact-bits cache key),
observability (`/stats`, pops/search-ms histograms, cache-miss attribution), the entire
hot-loop bundle, full-width SIMD ALT (**−53…67% pops, −60% wall on the incident
class**), component precheck (impossible queries → µs rejections), scale-aware budgets,
bidirectional multi-source, canonical pruning Stage 1+2a, typed payloads, profile
cache, context pool. Aggregate: medium routes went **5.8–25.2 ms → 0.15–0.64 ms**.

Measured-and-rejected (full table in roadmap v2 Appendix A — keep honoring it):
bucketed tie-break (`NAVPATH_TIEBREAK_BUCKET_MS`, 2–20× *more* pops post-full-width),
Dial/bucket queues, per-compare packed heap keys, goal-bounding under eligibility,
dead-end tagging, Eytzinger lookup, plain PGO/BOLT (memory-bound), `NAVPATH_ALT_HEAP`
hugepages at 1.1M, per-seed cache sub-caches, always-bidir.

---

## 3. Headline: the jitter/seed suggestion — **confirmed, with numbers**

The suggestion ("remove the jitter/seed to make paths faster") is **true**, and the
evidence is already in the tree. What the seed does today (`search.rs:49`
`edge_jitter`): a request-supplied `seed: Option<u64>` adds deterministic per-edge
jitter in [0, 0.1) ms to every relaxed edge so equal-cost ties resolve differently per
seed — path *variety*, nothing else. Its measured costs:

1. **It costs the route cache — the dominant cost.** `seed` is part of
   `RouteCacheKey`; the production client sends a fresh seed per request, so the hit
   rate for the dominant traffic class is ~0. Measured 2026-07-31 (cache-miss
   attribution work): on a repeating pair with random seeds,
   `NAVPATH_CACHE_IGNORE_SEED=1` converted **11 of 12 requests into hits, ~118 ms →
   ~0.3–0.9 ms**. The `cache_miss_seed` counter in `/stats` now reports *exactly* how
   many production requests the policy would convert.
2. **It disables canonical pruning outright.** `canonical_ctx` returns `None` whenever
   `seed.is_some()` (`search.rs:185`) — jitter breaks the uniform-cost premise of the
   successor table. Seeded traffic therefore gets nothing from Phase E today, and will
   get nothing from Stage 2b/3 (JPS) either.
3. **It inflates the search itself ~1.0–2.8×.** Jitter scatters the exact f-ties the
   high-g tie-break collapses, so seeded searches expand more and churn the heap more.
   Honest figure (corrected 2026-07-31): **1.0–2.8× more pops**, not the order of
   magnitude an earlier cache-contaminated measurement suggested. Historically, seeds
   alone pushed a real route from ~400k pops to over budget — both production budget
   incidents were the seeded shape.
4. **It carries structural complexity:** the 3-rung retry ladder, the `seed_dropped`
   degradation contract, seeded perf-gate tolerance bands of 30–40%, a per-edge
   `match params.seed` branch in every relax closure, and the seeded arms of the
   replay/bench corpus.

### The decision ladder (pick the deepest rung the client owner signs off on)

| Rung | Change | Wins | Loses |
|---|---|---|---|
| **3a. Flip `NAVPATH_CACHE_IGNORE_SEED=1`** (env only, no code) | Serve cached results to any seed | Repeat-pair traffic ~118 ms → sub-ms; zero engine risk; reversible instantly | Per-seed tie variety on cache hits (paths differ only among *equal-cost* routes; jitter envelope ≤ 0.1 ms/edge) |
| **3b. Client stops sending seeds** (client change; server already handles `seed: null`) | All traffic becomes unseeded | Canonical pruning engages for everything (+14% bidir wall today, 1.30× on gated in-process; multiplies when JPS lands); 1.0–2.8× fewer pops; retry ladder rarely fires; cache works naturally | All path variety |
| **3c. Remove seed/jitter from the engine** (API + engine change) | Delete `edge_jitter`, the seed params, the seeded ladder rungs, seeded bench/replay arms | Simplest possible hot loop (no per-edge seed branch); every future optimization applies to 100% of traffic; perf-gate bands tighten | API compatibility; variety permanently gone unless re-added post-search |
| **3d. (If variety must stay) replace, don't remove** | Segment-local jitter on macro/teleport legs only, or post-search equal-cost tie shuffling during reconstruction | Keeps variety while leaving walk-grid costs uniform → canonical pruning stays sound for seeded traffic | Design + proof work; parked in roadmap v2 Stage 4 "until asked" — this is the ask |

Recommended sequencing: **3a immediately** (it is blocked on a product decision only —
the measurement gate closed 2026-07-31), then take 3b/3c as a client-contract
conversation, with 3d as the fallback if variety is a hard requirement. Note the
truthful framing for that conversation: *the real price of a seed is the cache, not the
search* — and what variety buys is only which of several exactly-equal-cost paths is
returned.

If seeds are removed at any rung ≥ 3b, re-rank §5: the JPS ladder stops being
"unseeded-only traffic" and becomes the biggest remaining lever for all traffic.

---

## 4. Quick engineering wins (hours each, no algorithmic risk)

Verified-present issues, in rough value order:

1. **`/admin/reload` runs ~90+ ms of load work on the async reactor**
   (`routes.rs:1470-1514`: `Snapshot::open`, provider build ~8 ms, canonical grid
   ~83 ms, component graph — none wrapped in `spawn_blocking`). Wrap it; every
   in-flight request on that worker stalls today.
2. **Per-request clones out of `Arc<ProfileArtifacts>`**
   (`engine_adapter.rs:834-836`, `:936-937`): `eligible_globals.clone()`,
   `fairy_sources.clone()`, `fairy_dests.clone()` into `view.extra` on every request —
   partially defeats the 5.4 profile cache. Make `ExtraEdges` hold `Arc<[…]>`/borrows.
3. **`build_req_id_to_tag_index` rebuilt per payload** (`routes.rs:776`, ~186-entry
   HashMap, snapshot-constant) plus two per-payload HashMaps over ~124 globals
   (`routes.rs:781-796`). Hoist all three into `SnapshotState`.
4. **`macro_lookup` SipHash probe per path window** (`routes.rs:820`, ~3000 probes on a
   long route). Precompute a slot map or switch to a faster hasher.
5. **`Json(resp)` serialization on the reactor** (`routes.rs:1464`) — serialize inside
   the existing `spawn_blocking` and return bytes.
6. **Search permit held through payload build + cache put** (`routes.rs:1206` binds
   `_permit` to end of scope; the context pair already returns earlier). Drop it after
   the search so payload-heavy requests don't starve search admission.
7. **`.tcp_nodelay(true)`** on `axum::serve` (`main.rs:86`) — one line, still unlanded.
8. **`[profile.dev.package.navpath-core] opt-level = 3`** — engine iteration currently
   pays fat-LTO/CGU=1 release builds for every experiment (roadmap §9.8, never landed).
9. **If seeds stay (no §3 rung lands): monomorphize the relax loop over the jitter
   policy** — the seeded/unseeded `match params.seed` branch runs per relaxed edge
   (`search.rs:503`, `:698`, `:809`, `:888`); a generic parameter compiles it out of
   both variants (roadmap 2.6 item (2), the one piece of that bundle that never
   landed). Pointless under 3c (delete the branch instead).

---

## 5. Measurement first — the gate for everything else

1. **Re-bless on the deployed snapshot.** Baselines were blessed on the 64-landmark
   330 MB snapshot; the live one is **32 landmarks / 1,133,793 nodes / 189 MB**. Run
   the full suite + `scripts/perf-gate.sh bless`, and **A/B 32 vs 64 landmarks** while
   at it: the −53…67% pop reductions that justified full-width ALT were measured on 64;
   32 halves the table (memory win ~145 MB) but weakens every bound. If 64 measures
   better on the seeded/gated classes, rebuild at 64 — the README's build command
   already says `--landmarks 64`.
2. **Land the in-flight AVX-512 `h_active`** (uncommitted in `heuristics.rs`): gate on
   the existing `avx512_full_row_matches_portable` bit-identity test + replay corpus
   (costs AND pops unchanged) + `heuristic/h_active_1k` bench, then commit.
3. **2.10 NodeState hot/cold split** (`{g,gen}` / `{parent,h}`) — the one hot-loop item
   from roadmap §2 never landed. It is both a speed candidate (failed relaxes and
   stale-pop checks touch half the record) *and* the leading suspect for the ±34–45%
   identical-code variance on bidir classes (cache-set aliasing between the two ~18 MB
   context arrays). Fixing it would tighten every future perf gate. In-process A/B
   (`diff_bidir`, `diff_canonical`), not criterion, is the accept signal.
4. **Production telemetry before structural work:** `/stats` already carries
   `pops_f/pops_b`, `cache_miss_seed`, and log2 histograms. Pull a week of real traffic
   and answer: (a) how much traffic the seed policy would convert (§3a sizing), (b)
   whether a weak-backward tail exists (gates 4.3/NBS), (c) the real shape of the gated
   tail (gates §6).

---

## 6. Algorithmic levers (larger, gated)

1. **Phase E Stage 2b → Stage 3 (canonical tie-pruning → JPS jumping).** The one big
   un-taken engine lever: 5–30× pop reduction on open-field walk-dominated routes;
   every piece of substrate exists (mask grid, O(1) direction→slot addressing,
   forced-stop inputs, differential + shadow harnesses). Blocked on a real proof
   obligation — the **parent-race** on equal-cost relaxations (`canonical.rs:22-27`);
   land 2b first or jump with strict-only stopping sets. **Priority multiplies with
   §3:** today this serves only unseeded traffic (≈ none in production); after any §3
   rung ≥ b it serves everything.
2. **The gated-flood tail (190–220 ms).** The component precheck removed *unreachable*
   floods; reachable-but-heavily-gated goals still flood. The designed answer is the
   **CRP-style eligibility-aware overlay** (roadmap §8) — deliberately gated on 4M-map
   measurements, and that gate still stands. Interim mitigations worth measuring now:
   budget/deadline tuning for the gated shape, and checking whether the demotion policy
   misroutes it (production `pops_f/pops_b` from §5.4 decides).
3. **4.3 NBS / stronger bidir stopping** — only if §5.4's telemetry shows a
   weak-backward tail the 0.5-ratio demotion misses.
4. **3.3 landmark placement refinement, 7.2/7.3/7.6 builder scale work, snapshot-v9
   Hilbert renumbering, CRP overlay commit** — all remain gated on a real 4M dataset;
   nothing changed.

---

## 7. Validation protocol (unchanged, mandatory per change)

```sh
cargo run --release -p navpath-service --example replay          # golden corpus, uni+bidir+virtual × seeds
tools/invariance_check.sh                                        # cost invariance across landmark counts
python3 tools/verify_actions.py 8080                             # payload/action invariants (service running)
scripts/perf-gate.sh check                                       # criterion vs blessed baselines
cargo run --release -p navpath-core --example diff_bidir         # in-process A/B — the real signal for bidir classes
cargo run --release -p navpath-core --example diff_canonical     # (--gated for the flood class)
```

Bit-exactness rules carried forward: cost-exact changes must reproduce costs AND pop
counts on the corpus; bounded-suboptimal changes (any §3 rung, if variety semantics
change) ship behind an env flag with a `replay`-verified bound; criterion medians for
bidir/gated/seeded classes are only trusted inside the measured per-group tolerance
table in `scripts/perf-gate.sh`.
