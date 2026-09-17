use std::sync::Arc;
use std::sync::OnceLock;

use axum::{extract::{Query, State}, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use navpath_core::eligibility::{build_mask_from_u32, ClientValue};

use crate::{engine_adapter, AppState, SnapshotState};

/// Optional path to dump each route response as pretty JSON, controlled by the
/// `NAVPATH_DUMP_RESULT` env var. Disabled unless the var is set to a non-empty path.
/// Cached once so the hot path never performs an env lookup.
fn result_dump_path() -> Option<&'static std::path::Path> {
    static DUMP_PATH: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    DUMP_PATH
        .get_or_init(|| match std::env::var("NAVPATH_DUMP_RESULT") {
            Ok(p) if !p.trim().is_empty() => Some(std::path::PathBuf::from(p)),
            _ => None,
        })
        .as_deref()
}

/// Per-request wall-clock deadline from `NAVPATH_ROUTE_TIMEOUT_MS` (default 10s, 0
/// disables by using a very large timeout). Cached once.
fn route_deadline() -> std::time::Duration {
    static DL: OnceLock<std::time::Duration> = OnceLock::new();
    *DL.get_or_init(|| {
        let ms = std::env::var("NAVPATH_ROUTE_TIMEOUT_MS").ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(10_000);
        if ms == 0 { std::time::Duration::from_secs(24 * 3600) } else { std::time::Duration::from_millis(ms) }
    })
}

#[derive(Debug, Deserialize)]
pub struct RequirementKV {
    pub key: String,
    pub value: serde_json::Value,
}

#[derive(Debug, Deserialize, Default)]
pub struct Profile {
    #[serde(default)]
    pub requirements: Vec<RequirementKV>,
}

fn req_has_quick_tele(reqs: &[RequirementKV]) -> bool {
    for r in reqs {
        if r.key.trim().eq_ignore_ascii_case("hasQuickTele") {
            if r.value.as_i64() == Some(1) || r.value.as_u64() == Some(1) {
                return true;
            }
            if r.value.as_bool() == Some(true) {
                return true;
            }
            if r.value.as_str().map(|s| s.trim()) == Some("1") {
                return true;
            }
        }
    }
    false
}

/// Parse a macro edge's metadata once and return it if the profile satisfies the edge's
/// requirements (missing/unparseable metadata counts as allowed, matching the search's
/// fail-open handling of empty requirement lists). None = edge not allowed.
fn macro_edge_meta_if_allowed(
    snap: &navpath_core::Snapshot,
    macro_idx: usize,
    req_id_to_tag_idx: &crate::FxHashMap<u32, usize>,
    mask: &navpath_core::eligibility::EligibilityMask,
) -> Option<serde_json::Value> {
    let Some(bytes) = snap.macro_meta_at(macro_idx) else { return Some(serde_json::json!({})); };
    let Ok(val) = serde_json::from_slice::<serde_json::Value>(bytes) else { return Some(serde_json::json!({})); };
    if let Some(arr) = val.get("requirements").and_then(|v| v.as_array()) {
        for ridv in arr {
            let Some(rid) = ridv.as_u64() else { continue; };
            let Some(&tag_idx) = req_id_to_tag_idx.get(&(rid as u32)) else { return None; };
            if !mask.is_satisfied(tag_idx) {
                return None;
            }
        }
    }
    Some(val)
}

#[derive(Debug, Deserialize, Default)]
pub struct RouteOptions {
    #[serde(default)]
    pub return_geometry: bool,
    #[serde(default)]
    pub only_actions: bool,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SurgeConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub charges: u32,
    #[serde(default = "default_cooldown_ms")]
    pub cooldown_ms: f64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DiveConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub available_in_ms: f64,
    #[serde(default = "default_cooldown_ms")]
    pub cooldown_ms: f64,
}

fn default_cooldown_ms() -> f64 {
    20400.0
}

impl Default for SurgeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            charges: 0,
            cooldown_ms: default_cooldown_ms(),
        }
    }
}

impl Default for DiveConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            available_in_ms: 0.0,
            cooldown_ms: default_cooldown_ms(),
        }
    }
}

/// Minimum tiles required to use surge (not worth cooldown for less)
const MIN_SURGE_TILES: usize = 5;
/// Minimum tiles required to use dive (very aggressive - use whenever possible)
const MIN_DIVE_TILES: usize = 2;
/// Maximum tiles surge/dive can cover
const MAX_ABILITY_TILES: usize = 10;
/// Minimum tiles to walk in surge direction before using surge (to establish facing).
/// Waived when a dive along the same heading immediately precedes the surge — the dive
/// already leaves the character facing that way.
const MIN_WALK_BEFORE_SURGE: usize = 3;

/// Represents direction for surge (must be straight line)
#[derive(Debug, Clone, Copy, PartialEq)]
enum Direction {
    North,      // y increases
    South,      // y decreases
    East,       // x increases
    West,       // x decreases
    NorthEast,  // x+, y+
    NorthWest,  // x-, y+
    SouthEast,  // x+, y-
    SouthWest,  // x-, y-
}

impl Direction {
    fn from_delta(dx: i32, dy: i32) -> Option<Self> {
        match (dx.signum(), dy.signum()) {
            (0, 1) => Some(Direction::North),
            (0, -1) => Some(Direction::South),
            (1, 0) => Some(Direction::East),
            (-1, 0) => Some(Direction::West),
            (1, 1) => Some(Direction::NorthEast),
            (-1, 1) => Some(Direction::NorthWest),
            (1, -1) => Some(Direction::SouthEast),
            (-1, -1) => Some(Direction::SouthWest),
            _ => None,
        }
    }
}

/// A `{min, max}` coordinate block (always a single tile today, min == max).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct MinMax {
    pub min: [i32; 3],
    pub max: [i32; 3],
}

impl MinMax {
    fn point(x: i32, y: i32, p: i32) -> Self {
        Self { min: [x, y, p], max: [x, y, p] }
    }
}

/// `node` block on macro actions: the edge kind and its snapshot id.
#[derive(Debug, Serialize)]
pub struct NodeRef {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub id: u32,
}

/// One walked tile.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct MoveAction {
    #[serde(rename = "type")]
    pub kind: &'static str, // "move"
    pub to: [i32; 3],
    pub cost_ms: f64,
}

impl MoveAction {
    #[inline]
    fn dest(&self) -> (i32, i32, i32) {
        (self.to[0], self.to[1], self.to[2])
    }
}

/// Surge/dive inserted by [`optimize_with_surge_dive`]. `cost_ms` stays an integer
/// (it was a `0` literal in the old `json!` construction).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct AbilityAction {
    #[serde(rename = "type")]
    pub kind: &'static str, // "surge" | "dive"
    pub from: [i32; 3],
    pub to: [i32; 3],
    pub cost_ms: u32, // always 0
    pub tiles_covered: usize,
}

/// A macro edge step (door/lodestone/npc/object/item/ifslot/use_on/teleport).
#[derive(Debug, Serialize)]
pub struct MacroAction {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub from: MinMax,
    pub to: MinMax,
    pub cost_ms: f64,
    pub node: NodeRef,
    /// Owned because this subtree is MUTATED per action (door_direction insertion,
    /// db_row removal); it is parsed fresh from the snapshot's metadata bytes.
    pub metadata: serde_json::Value,
}

/// Serialize an `Arc<Value>` by reading through it (serde's blanket `Arc` impl is
/// behind the `rc` feature, which this crate doesn't enable).
fn serialize_arc_value<S: serde::Serializer>(
    v: &Arc<serde_json::Value>,
    s: S,
) -> Result<S::Ok, S::Error> {
    v.as_ref().serialize(s)
}

/// A global teleport step taken from the on-graph origin.
#[derive(Debug, Serialize)]
pub struct GlobalAction {
    #[serde(rename = "type")]
    pub kind: String, // steps[0].kind from the metadata, or "global_teleport"
    pub from: MinMax,
    pub to: MinMax,
    pub cost_ms: f64,
    /// Pass-through of the teleport's load-time-parsed metadata — serialized straight
    /// from the shared Arc, never deep-cloned.
    #[serde(serialize_with = "serialize_arc_value")]
    pub metadata: Arc<serde_json::Value>,
}

/// `from` block of a fairy ring action (carries the source ring's object id).
#[derive(Debug, Serialize)]
pub struct FairyFrom {
    pub min: [i32; 3],
    pub max: [i32; 3],
    pub object_id: u64,
}

/// A fairy ring hop.
#[derive(Debug, Serialize)]
pub struct FairyAction {
    #[serde(rename = "type")]
    pub kind: &'static str, // "fairy_ring"
    pub from: FairyFrom,
    pub to: MinMax,
    pub code: String,
    pub cost_ms: f64,
    pub metadata: serde_json::Value,
}

/// Fallback for a path edge of unknown kind: generic teleport, integer zero cost
/// (matching the old `json!` literal), no node/metadata.
#[derive(Debug, Serialize)]
pub struct TeleportAction {
    #[serde(rename = "type")]
    pub kind: &'static str, // "teleport"
    pub from: MinMax,
    pub to: MinMax,
    pub cost_ms: u32, // always 0
}

/// Synthetic first action when the requested start coordinate is off-graph and the
/// route enters through a global teleport. `cost_ms` is `serde_json::Number` because
/// it is the integer `0` until a winning entry teleport supplies an f64 cost — the
/// old code emitted exactly those two shapes.
#[derive(Debug, Serialize)]
pub struct VirtualStartAction {
    #[serde(rename = "type")]
    pub kind: String,
    pub from: MinMax,
    pub to: MinMax,
    pub cost_ms: serde_json::Number,
    pub metadata: serde_json::Value,
}

/// Typed response actions (roadmap 5.3): every shape `build_route_payload` and
/// `optimize_with_surge_dive` emit, serialized directly instead of assembling
/// per-action `serde_json::Value` trees. Untagged: each variant carries its own
/// `type` field (the tag value is dynamic for macro/global actions).
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Action {
    Move(MoveAction),
    Ability(AbilityAction),
    Macro(Box<MacroAction>),
    Global(Box<GlobalAction>),
    Fairy(Box<FairyAction>),
    Teleport(TeleportAction),
    VirtualStart(Box<VirtualStartAction>),
}

impl Action {
    /// The action's cost in ms (0.0 where the serialized form is the integer 0),
    /// mirroring the old `get("cost_ms").as_f64().unwrap_or(0.0)`.
    fn cost_ms_f64(&self) -> f64 {
        match self {
            Action::Move(a) => a.cost_ms,
            Action::Ability(a) => a.cost_ms as f64,
            Action::Macro(a) => a.cost_ms,
            Action::Global(a) => a.cost_ms,
            Action::Fairy(a) => a.cost_ms,
            Action::Teleport(a) => a.cost_ms as f64,
            Action::VirtualStart(a) => a.cost_ms.as_f64().unwrap_or(0.0),
        }
    }

    /// Destination tile: the `to` array for moves/abilities, `to.min` for the
    /// min/max-block actions — the same lookup the old code did on JSON values.
    fn to_coords(&self) -> (i32, i32, i32) {
        let a = match self {
            Action::Move(a) => &a.to,
            Action::Ability(a) => &a.to,
            Action::Macro(a) => &a.to.min,
            Action::Global(a) => &a.to.min,
            Action::Fairy(a) => &a.to.min,
            Action::Teleport(a) => &a.to.min,
            Action::VirtualStart(a) => &a.to.min,
        };
        (a[0], a[1], a[2])
    }
}

/// Find the first available surge charge and return its index
fn find_available_surge_charge(charges: &[f64], elapsed_ms: f64) -> Option<usize> {
    charges.iter().position(|&available_at| available_at <= elapsed_ms)
}

/// Calculate straight-line distance between two points
fn straight_line_distance(x1: i32, y1: i32, x2: i32, y2: i32) -> f64 {
    let dx = (x2 - x1) as f64;
    let dy = (y2 - y1) as f64;
    (dx * dx + dy * dy).sqrt()
}

/// Range predicate shared by dive and surge: both abilities move the character directly
/// to the target tile, so only the straight-line hop matters, not the walked path.
///
/// Surge used to have NO range check. Its only test was that the replaced run was roughly
/// straight (`tiles <= straight_dist + 2.0`), which a run of 10 DIAGONAL steps passes
/// while displacing 10*sqrt(2) = 14.14 tiles — beyond the abilities' reach, and beyond
/// what this very predicate was already refusing for dive over the same endpoints. The two
/// abilities disagreed about what was reachable, and the client was handed surge hops it
/// could not perform (measured on the golden corpus: spans of 10.77 to 14.14 tiles on 12
/// of 18 payloads).
fn is_valid_ability_hop(start: (i32, i32, i32), end: (i32, i32, i32)) -> bool {
    if start.2 != end.2 {
        return false; // Different planes
    }
    let straight_dist = straight_line_distance(start.0, start.1, end.0, end.1);
    straight_dist <= (MAX_ABILITY_TILES as f64) + 0.5
}

/// Optimize actions by inserting surge and dive abilities. Consumes the action list:
/// non-move actions are moved (never cloned) into the output, and walked tiles are
/// `Copy` structs — no per-action JSON re-parsing or cloning (roadmap 5.3).
///
/// `route_origin` is the tile the character actually stands on before the first action
/// (`path[0]`). It is REQUIRED for correctness whenever the route opens with walk steps:
/// actions only carry their destination (`to`), so the first move's origin exists nowhere
/// in the list. Without it the optimizer fell back to the first move's *destination*,
/// which reported ability origins one tile ahead of the character, sized each ability
/// against the wrong origin tile (a dive could legally span 11 tiles), and silently
/// dropped the opening walk step from the payload.
fn optimize_with_surge_dive(
    actions: Vec<Action>,
    surge_config: &SurgeConfig,
    dive_config: &DiveConfig,
    route_origin: Option<(i32, i32, i32)>,
) -> Vec<Action> {
    // If neither ability is enabled, return as-is
    if !surge_config.enabled && !dive_config.enabled {
        return actions;
    }

    // Track cooldowns: each charge has its own "available_at" time (all start at 0)
    let mut surge_charges: Vec<f64> = vec![0.0; surge_config.charges as usize];
    let mut dive_available_at: f64 = dive_config.available_in_ms;

    let mut result: Vec<Action> = Vec::with_capacity(actions.len());
    let mut elapsed_ms: f64 = 0.0;
    let mut iter = actions.into_iter().peekable();

    while let Some(action) = iter.next() {
        // Only process sequences of "move" actions
        let Action::Move(first_move) = action else {
            // Add non-move action and accumulate its cost
            elapsed_ms += action.cost_ms_f64();
            result.push(action);
            continue;
        };

        // Where the character stands before this move run: the previous action's
        // destination, or — at the head of the route — the caller-supplied origin tile.
        let start_pos = result.last().map(Action::to_coords).or(route_origin);

        // Collect consecutive move actions
        let mut move_sequence: Vec<MoveAction> = vec![first_move];
        while let Some(Action::Move(m)) = iter.peek() {
            move_sequence.push(*m);
            iter.next();
        }

        // Now try to find surge/dive opportunities within this move sequence
        let mut seq_idx = 0;
        while seq_idx < move_sequence.len() {
            // Determine current position
            let mut current_pos = if seq_idx == 0 {
                // Last-resort fallback (origin unknown AND no preceding action): keeps the
                // old behaviour rather than panicking. Callers always pass `route_origin`.
                start_pos.unwrap_or_else(|| move_sequence[0].dest())
            } else {
                move_sequence[seq_idx - 1].dest()
            };

            let mut dive_used = false;
            let mut dive_dir: Option<Direction> = None;
            let mut surge_used = false;

            // Try dive FIRST - it has no facing requirement, can be used anytime when off cooldown
            let remaining_tiles = move_sequence.len() - seq_idx;
            if remaining_tiles >= MIN_DIVE_TILES && dive_config.enabled && dive_available_at <= elapsed_ms {
                let mut best_dive_count = 0;

                for dive_count in (MIN_DIVE_TILES..=MAX_ABILITY_TILES.min(remaining_tiles)).rev() {
                    let end_pos = move_sequence[seq_idx + dive_count - 1].dest();
                    if is_valid_ability_hop(current_pos, end_pos) {
                        best_dive_count = dive_count;
                        break;
                    }
                }

                if best_dive_count >= MIN_DIVE_TILES {
                    let end_idx = seq_idx + best_dive_count - 1;
                    let (end_x, end_y, end_p) = move_sequence[end_idx].dest();

                    result.push(Action::Ability(AbilityAction {
                        kind: "dive",
                        from: [current_pos.0, current_pos.1, current_pos.2],
                        to: [end_x, end_y, end_p],
                        cost_ms: 0,
                        tiles_covered: best_dive_count,
                    }));

                    dive_available_at = elapsed_ms + dive_config.cooldown_ms;
                    seq_idx = end_idx + 1;
                    dive_used = true;
                    dive_dir = Direction::from_delta(end_x - current_pos.0, end_y - current_pos.1);

                    // Update current_pos after dive
                    current_pos = (end_x, end_y, end_p);
                }
            }

            // Try surge (requires facing direction from prior moves)
            let remaining_tiles = move_sequence.len() - seq_idx;
            if remaining_tiles >= MIN_SURGE_TILES && surge_config.enabled && !surge_charges.is_empty() {
                if let Some(charge_idx) = find_available_surge_charge(&surge_charges, elapsed_ms) {
                    let mut best_surge_count = 0;
                    let mut best_surge_dir: Option<Direction> = None;

                    for tiles in (MIN_SURGE_TILES..=MAX_ABILITY_TILES.min(remaining_tiles)).rev() {
                        let (end_x, end_y, end_p) = move_sequence[seq_idx + tiles - 1].dest();

                        // Same plane AND within the ability's reach — the check surge was
                        // missing (see `is_valid_ability_hop`).
                        if !is_valid_ability_hop(current_pos, (end_x, end_y, end_p)) {
                            continue;
                        }

                        let straight_dist = straight_line_distance(current_pos.0, current_pos.1, end_x, end_y);

                        // ...and the replaced run must be near-straight, or surging it
                        // would cut a corner the walk deliberately took.
                        if (tiles as f64) <= straight_dist + 2.0 {
                            let dx = end_x - current_pos.0;
                            let dy = end_y - current_pos.1;
                            if let Some(dir) = Direction::from_delta(dx, dy) {
                                best_surge_count = tiles;
                                best_surge_dir = Some(dir);
                                break;
                            }
                        }
                    }

                    // Establish facing: a dive along the same heading already turns the
                    // character, otherwise fall back to counting prior moves.
                    let mut facing_established = false;
                    if let Some(surge_dir) = best_surge_dir {
                        if dive_used && dive_dir == Some(surge_dir) {
                            facing_established = true;
                        } else {
                            let mut prior_moves_in_direction = 0;
                            for idx in (0..result.len()).rev() {
                                let Action::Move(prev_move) = &result[idx] else {
                                    break;
                                };
                                let prev_to = prev_move.dest();
                                // The action list stores destinations only, so a move's
                                // origin is the previous action's destination — and for
                                // the very first action, the route origin.
                                let prev_from = if idx == 0 {
                                    let Some(o) = route_origin else { break };
                                    o
                                } else {
                                    result[idx - 1].to_coords()
                                };

                                let dx = prev_to.0 - prev_from.0;
                                let dy = prev_to.1 - prev_from.1;
                                let move_dir = Direction::from_delta(dx, dy);

                                if move_dir == Some(surge_dir) {
                                    prior_moves_in_direction += 1;
                                } else {
                                    break;
                                }

                                if prior_moves_in_direction >= MIN_WALK_BEFORE_SURGE {
                                    break;
                                }
                            }
                            facing_established = prior_moves_in_direction >= MIN_WALK_BEFORE_SURGE;
                        }
                    }

                    if best_surge_count >= MIN_SURGE_TILES && facing_established {
                        let end_idx = seq_idx + best_surge_count - 1;
                        let (end_x, end_y, end_p) = move_sequence[end_idx].dest();

                        result.push(Action::Ability(AbilityAction {
                            kind: "surge",
                            from: [current_pos.0, current_pos.1, current_pos.2],
                            to: [end_x, end_y, end_p],
                            cost_ms: 0,
                            tiles_covered: best_surge_count,
                        }));

                        surge_charges[charge_idx] = elapsed_ms + surge_config.cooldown_ms;
                        seq_idx = end_idx + 1;
                        surge_used = true;
                    }
                }
            }

            // If neither ability was used, walk one tile
            if !dive_used && !surge_used {
                let m = move_sequence[seq_idx];
                elapsed_ms += m.cost_ms;
                result.push(Action::Move(m));
                seq_idx += 1;
            }
        }
    }

    result
}

#[derive(Debug, Deserialize, Default)]
pub struct NodeTile { pub wx: i32, pub wy: i32, pub plane: i32 }

#[derive(Debug, Deserialize)]
pub struct RouteRequest {
    // Back-compat: allow direct ids
    #[serde(default)] pub start_id: Option<u32>,
    #[serde(default)] pub goal_id: Option<u32>,
    // Spec format: coordinates
    #[serde(default)] pub start: Option<NodeTile>,
    #[serde(default)] pub goal: Option<NodeTile>,
    #[serde(default)] pub profile: Profile,
    #[serde(default)] pub options: RouteOptions,
    // Surge and Dive abilities
    #[serde(default)] pub surge: SurgeConfig,
    #[serde(default)] pub dive: DiveConfig,
    /// Optional seed for path randomization. Same seed = same path. Different seeds = potentially different paths.
    #[serde(default)] pub seed: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    /// False while the startup warm-up runs (HTTP 503 as well); see `AppState::ready`.
    pub ready: bool,
    pub version: u32,
    pub snapshot_hash: Option<String>,
    pub loaded_at: u64,
    pub counts: Option<Counts>,
}

#[derive(Debug, Serialize)]
pub struct Counts {
    pub nodes: u32,
    pub walk_edges: u32,
    pub macro_edges: u32,
    pub req_tags: u32,
    pub landmarks: u32,
}

#[derive(Debug, Serialize)]
pub struct RouteResponse {
    pub found: bool,
    pub cost: f32,
    #[serde(skip_serializing_if = "Vec::is_empty")] pub path: Vec<u32>,
    pub length_tiles: usize,
    pub duration_ms: u128,
    /// Same clock as `duration_ms`, in microseconds (sub-ms routes read as 0 there).
    pub duration_us: u64,
    /// Present when the search gave up rather than proving its answer
    /// ("budget_exceeded" or "cancelled"). With found=false the goal may still be
    /// reachable; with found=true the returned path is valid but was not proven
    /// optimal (the search was truncated mid-proof). Absent on proven outcomes.
    #[serde(skip_serializing_if = "Option::is_none")] pub reason: Option<String>,
    /// Present when a request-level guarantee was traded for an answer. Currently only
    /// "seed_dropped": the request sent a seed, both seeded attempts exhausted their
    /// budgets, and the served route is the deterministic unseeded optimum.
    #[serde(skip_serializing_if = "Option::is_none")] pub degraded: Option<String>,
    /// Pre-serialized in the blocking task (`RawValue` embeds verbatim), so the multi-KB
    /// action list / geometry never serialize on the reactor thread. Bytes are identical
    /// to serializing the typed values here — same serializer, same values.
    #[serde(skip_serializing_if = "Option::is_none")] pub actions: Option<Box<serde_json::value::RawValue>>,
    #[serde(skip_serializing_if = "Option::is_none")] pub geometry: Option<Box<serde_json::value::RawValue>>,
}

#[derive(Debug, Deserialize)]
pub struct TileExistsQuery {
    pub x: i32,
    pub y: i32,
    pub plane: i32,
}

#[derive(Debug, Serialize)]
pub struct TileExistsResponse {
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub walk_mask: Option<u8>,
}

pub async fn health(State(state): State<AppState>) -> (StatusCode, Json<HealthResponse>) {
    let ready = state.ready.load(std::sync::atomic::Ordering::Acquire);
    let cur = state.current.load();
    let counts = cur.snapshot.as_ref().map(|s| s.counts()).map(|c| Counts {
        nodes: c.nodes,
        walk_edges: c.walk_edges,
        macro_edges: c.macro_edges,
        req_tags: c.req_tags,
        landmarks: c.landmarks,
    });
    let version = cur.snapshot.as_ref().map(|s| s.manifest().version).unwrap_or(0);
    let body = Json(HealthResponse {
        ready,
        version,
        snapshot_hash: cur.snapshot_hash_hex.clone(),
        loaded_at: cur.loaded_at_unix,
        counts,
    });
    (if ready { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE }, body)
}

pub async fn tile_exists(
    State(state): State<AppState>,
    Query(params): Query<TileExistsQuery>,
) -> Result<Json<TileExistsResponse>, (StatusCode, String)> {
    let cur = state.current.load();

    let Some(snap) = cur.snapshot.as_ref() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "snapshot not loaded".into(),
        ));
    };

    match snap.find_node(params.x, params.y, params.plane) {
        Some(node_id) => {
            Ok(Json(TileExistsResponse {
                exists: true,
                node_id: Some(node_id),
                walk_mask: None, // tiles.bin not currently loaded into state
            }))
        }
        None => Ok(Json(TileExistsResponse {
            exists: false,
            node_id: None,
            walk_mask: None,
        })),
    }
}

/// Maximum start->goal range for `/reachable` (Chebyshev tiles, i.e. the in-game
/// "within N tiles" square).
const REACHABLE_RANGE_TILES: i32 = 20;

#[derive(Debug, Deserialize)]
pub struct ReachableQuery {
    pub sx: i32,
    pub sy: i32,
    pub splane: i32,
    pub gx: i32,
    pub gy: i32,
    pub gplane: i32,
}

#[derive(Debug, Serialize)]
pub struct ReachableResponse {
    pub reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

/// Walk-only proximity check: true iff the goal is within
/// [`REACHABLE_RANGE_TILES`] of the start AND a pure walk path connects them —
/// no macro edges (doors, stairs, teleports) allowed — without leaving the
/// endpoints' 20-tile neighbourhood.
///
/// Two-stage answer, both stages exact for their claim:
/// 1. Walk-component ids (built over walk edges ONLY — see the builder's
///    `walk_component_ids`) decide "connected by walks at all" in O(1). A goal
///    behind a closed door/fence is a different component and rejects here.
/// 2. A BFS over the walk CSR, restricted to tiles within range of either
///    endpoint (union keeps the answer symmetric), confirms the path is local.
///    Same component but only connected around a long detour (river bank,
///    cliff) rejects here. The region is at most ~61x61 tiles, so the whole
///    check is microseconds — no blocking task or search permit needed.
pub async fn reachable(
    State(state): State<AppState>,
    Query(q): Query<ReachableQuery>,
) -> Result<Json<ReachableResponse>, (StatusCode, String)> {
    let cur = state.current.load();
    let Some(snap) = cur.snapshot.as_ref() else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "snapshot not loaded".into()));
    };

    let deny = |reason: &'static str| {
        Ok(Json(ReachableResponse { reachable: false, reason: Some(reason) }))
    };

    // Walk edges never change plane; a cross-plane pair can't be walk-reachable.
    if q.splane != q.gplane {
        return deny("different_plane");
    }
    let cheb = (q.sx - q.gx).abs().max((q.sy - q.gy).abs());
    if cheb > REACHABLE_RANGE_TILES {
        return deny("out_of_range");
    }

    let Some(sid) = snap.find_node(q.sx, q.sy, q.splane) else {
        return deny("start_tile_not_found");
    };
    let Some(gid) = snap.find_node(q.gx, q.gy, q.gplane) else {
        return deny("goal_tile_not_found");
    };
    if sid == gid {
        return Ok(Json(ReachableResponse { reachable: true, reason: None }));
    }

    // Stage 1: no walk path exists AT ALL (the goal is only reachable through a
    // door/teleport, if at all) — reject without touching the grid.
    let comps = snap.comp_ids();
    if comps[sid as usize] != comps[gid as usize] {
        return deny("not_connected");
    }

    // Stage 2: BFS over walk edges, confined to tiles within range of either
    // endpoint. `visited` is a dense bitmap over the region's bounding box
    // (endpoints are <= range apart, so at most (3*range+1)^2 slots).
    let r = REACHABLE_RANGE_TILES;
    let (x0, y0) = (q.sx.min(q.gx) - r, q.sy.min(q.gy) - r);
    let width = (q.sx.max(q.gx) + r - x0 + 1) as usize;
    let height = (q.sy.max(q.gy) + r - y0 + 1) as usize;
    let idx = |x: i32, y: i32| (y - y0) as usize * width + (x - x0) as usize;
    let in_region = |x: i32, y: i32| {
        (x - q.sx).abs().max((y - q.sy).abs()) <= r
            || (x - q.gx).abs().max((y - q.gy).abs()) <= r
    };

    let offs = snap.walk_offsets();
    let dst = snap.walk_dst();
    let mut visited = vec![false; width * height];
    let mut queue = std::collections::VecDeque::with_capacity(64);
    visited[idx(q.sx, q.sy)] = true;
    queue.push_back(sid);
    while let Some(u) = queue.pop_front() {
        let (s, e) = (offs[u as usize] as usize, offs[u as usize + 1] as usize);
        for &v in &dst[s..e] {
            if v == gid {
                return Ok(Json(ReachableResponse { reachable: true, reason: None }));
            }
            let (vx, vy, vp) = snap.node_coord(v);
            if vp != q.splane || !in_region(vx, vy) {
                continue;
            }
            let i = idx(vx, vy);
            if !visited[i] {
                visited[i] = true;
                queue.push_back(v);
            }
        }
    }

    // A walk path exists (same component) but every one leaves the 20-tile
    // neighbourhood — e.g. the far bank of a river whose bridge is 50 tiles away.
    deny("no_path_in_range")
}

/// `steps[0].kind` from a global teleport's metadata (e.g. "lodestone", "npc"),
/// falling back to the generic tag.
fn global_step_kind(meta: &serde_json::Value) -> &str {
    meta.get("steps")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|s| s.get("kind"))
        .and_then(|v| v.as_str())
        .unwrap_or("global_teleport")
}

/// Build the optional actions/geometry payload for a found route. Runs inside the
/// request's blocking task so thousands of per-step constructions never stall the
/// async reactor threads. Emits typed [`Action`]s serialized directly by serde
/// (roadmap 5.3) — no per-tile/per-action `serde_json::Value` assembly.
#[allow(clippy::too_many_arguments)]
fn build_route_payload(
    snap: &navpath_core::Snapshot,
    globals: &[engine_adapter::GlobalTeleport],
    macro_lookup: &crate::FxHashMap<(u32, u32), Vec<u32>>,
    fairy_rings: &[engine_adapter::FairyRing],
    node_to_fairy_ring: &crate::FxHashMap<u32, usize>,
    req_id_to_tag_idx: &crate::FxHashMap<u32, usize>,
    mask: &navpath_core::eligibility::EligibilityMask,
    quick_tele: bool,
    return_geometry: bool,
    only_actions: bool,
    surge: &SurgeConfig,
    dive: &DiveConfig,
    virtual_start_from: Option<(i32, i32, i32)>,
    virtual_entry: Option<u32>,
    sid: u32,
    res: &navpath_core::SearchResult,
) -> (Option<Vec<Action>>, Option<Vec<[i32; 3]>>) {
    if !res.found {
        return (None, None);
    }

    let coord = |id: u32| -> (i32, i32, i32) { snap.node_coord(id) };

    let mut geometry: Option<Vec<[i32; 3]>> = None;
    if return_geometry {
        let mut geom: Vec<[i32; 3]> = Vec::with_capacity(res.path.len());
        for &id in &res.path {
            let (x, y, p) = coord(id);
            geom.push([x, y, p]);
        }
        geometry = Some(geom);
    }

    if !(only_actions || return_geometry) {
        return (None, geometry);
    }

    // Eligible global teleports for action annotation, from the metadata parsed once
    // at snapshot load (no per-request 113KB JSON re-parse). Metadata stays behind the
    // shared Arc — serialization reads through it, so nothing is deep-cloned here.
    let mut global_cost: crate::FxHashMap<u32, f32> = crate::FxHashMap::default();
    let mut global_meta: crate::FxHashMap<u32, Arc<serde_json::Value>> = crate::FxHashMap::default();
    for g in globals.iter() {
        if g.reqs.iter().any(|&idx| !mask.is_satisfied(idx)) {
            continue;
        }
        let mut cost = g.cost;
        if quick_tele && g.kind_first == 2 {
            cost = 2400.0;
        }
        let should_replace = global_cost.get(&g.dst).map(|c| cost < *c).unwrap_or(true);
        if should_replace {
            global_cost.insert(g.dst, cost);
            global_meta.insert(g.dst, g.meta.clone());
        }
    }

    let mut acts: Vec<Action> = Vec::with_capacity(res.path.len().saturating_sub(1));

    // If we used a virtual start (non-existent start coordinate), we'll need to add the teleport action later
    // after we determine the actual teleport type from the first real action
    let mut virtual_start_action: Option<VirtualStartAction> = None;
    if let Some((vsx, vsy, vsp)) = virtual_start_from {
        let entry_id = virtual_entry.unwrap_or(sid);
        let (actual_x, actual_y, actual_p) = coord(entry_id);
        virtual_start_action = Some(VirtualStartAction {
            kind: "global_teleport".to_string(),
            from: MinMax::point(vsx, vsy, vsp),
            to: MinMax::point(actual_x, actual_y, actual_p),
            cost_ms: serde_json::Number::from(0),
            metadata: serde_json::json!({"reason": "start_coordinate_not_found"}),
        });
    }

    for w in res.path.windows(2) {
        let (u, v) = (w[0], w[1]);
        let (x1, y1, p1) = coord(u);
        let (x2, y2, p2) = coord(v);

        if let Some(idxs) = macro_lookup.get(&(u, v)) {
            let mut chosen: Option<(usize, f32, serde_json::Value)> = None;
            for &idx_u32 in idxs {
                let idx = idx_u32 as usize;
                let Some(meta) = macro_edge_meta_if_allowed(snap, idx, &req_id_to_tag_idx, mask) else {
                    continue;
                };
                let mut cost_ms = snap.macro_w().get(idx).copied().unwrap_or(0.0);
                let k = snap.macro_kind_first().get(idx).copied().unwrap_or(0);
                if quick_tele && k == 2 {
                    cost_ms = 2400.0;
                }
                if chosen.as_ref().map_or(true, |(_, best_cost, _)| cost_ms < *best_cost) {
                    chosen = Some((idx, cost_ms, meta));
                }
            }
            let (idx, mut cost_ms, mut meta) = if let Some(best) = chosen {
                best
            } else {
                let idx = idxs.first().copied().unwrap_or(0) as usize;
                let meta = snap.macro_meta_at(idx)
                    .and_then(|b| serde_json::from_slice(b).ok())
                    .unwrap_or(serde_json::json!({}));
                (idx, snap.macro_w().get(idx).copied().unwrap_or(0.0), meta)
            };

            let k = snap.macro_kind_first().get(idx).copied().unwrap_or(0);
            let kid = snap.macro_id_first().get(idx).copied().unwrap_or(0);
            let kstr = match k {
                1 => "door",
                2 => "lodestone",
                3 => "npc",
                4 => "object",
                5 => "item",
                6 => "ifslot",
                7 => "poa_item",
                8 => "use_on",
                _ => "teleport",
            };
            if quick_tele && kstr == "lodestone" {
                cost_ms = 2400.0;
            }

            // For doors, compute dynamic approach direction (IN/OUT) using db_row tile_inside/tile_outside
            if kstr == "door" {
                // helper to extract [x,y,p] to tuple
                fn arr_to_tuple(v: &serde_json::Value) -> Option<(i32,i32,i32)> {
                    let a = v.as_array()?;
                    if a.len() != 3 { return None; }
                    let x = a[0].as_i64()? as i32;
                    let y = a[1].as_i64()? as i32;
                    let p = a[2].as_i64()? as i32;
                    Some((x,y,p))
                }
                if let Some(db_row) = meta.get("db_row") {
                    let tin = db_row.get("tile_inside").and_then(arr_to_tuple);
                    let tout = db_row.get("tile_outside").and_then(arr_to_tuple);
                    let from = (x1,y1,p1);
                    let to = (x2,y2,p2);
                    let dir = if tout.is_some() && Some(from) == tout { Some("IN") }
                              else if tin.is_some() && Some(from) == tin { Some("OUT") }
                              else if tin.is_some() && Some(to) == tin { Some("IN") }
                              else if tout.is_some() && Some(to) == tout { Some("OUT") }
                              else { None };
                    if let Some(d) = dir {
                        if let Some(obj) = meta.as_object_mut() {
                            obj.insert("door_direction".to_string(), serde_json::Value::String(d.to_string()));
                        }
                    }
                }
            }
            // Remove duplicated top-level db_row; it exists inside per-step entries already
            if let Some(obj) = meta.as_object_mut() {
                obj.remove("db_row");
            }
            acts.push(Action::Macro(Box::new(MacroAction {
                kind: kstr,
                from: MinMax::point(x1, y1, p1),
                to: MinMax::point(x2, y2, p2),
                cost_ms: cost_ms as f64,
                node: NodeRef { kind: kstr, id: kid },
                metadata: meta,
            })));
        } else {
            // Walk edge or unknown: check the snapshot's walk CSR directly
            // (degree <= 8, weight derived from the diagonal bitmap).
            let walk_w = snap.walk_edge_weight(u, v);

            if let Some(w_cost) = walk_w {
                acts.push(Action::Move(MoveAction {
                    kind: "move",
                    to: [x2, y2, p2],
                    cost_ms: w_cost.round() as f64,
                }));
            } else if let Some(gc) = global_cost.get(&v).copied() {
                let meta: Arc<serde_json::Value> = global_meta
                    .get(&v)
                    .cloned()
                    .unwrap_or_else(|| Arc::new(serde_json::json!({})));
                // Prefer the specific step kind (e.g., "lodestone", "npc") if present in metadata
                let kstr = global_step_kind(&meta);
                let cost_ms = if quick_tele && kstr == "lodestone" { 2400.0 } else { gc as f64 };
                let kind = kstr.to_string();
                acts.push(Action::Global(Box::new(GlobalAction {
                    kind,
                    from: MinMax::point(x1, y1, p1),
                    to: MinMax::point(x2, y2, p2),
                    cost_ms,
                    metadata: meta,
                })));
            } else {
                // Check if this is a fairy ring hop (u and v are both fairy ring nodes)
                let src_ring_idx = node_to_fairy_ring.get(&u);
                let dst_ring_idx = node_to_fairy_ring.get(&v);
                if let (Some(&src_idx), Some(&dst_idx)) = (src_ring_idx, dst_ring_idx) {
                    // Fairy ring teleport
                    let src_ring = &fairy_rings[src_idx];
                    let dst_ring = &fairy_rings[dst_idx];
                    let mut meta = serde_json::Map::new();
                    meta.insert("source_code".to_string(), serde_json::Value::String(src_ring.code.clone()));
                    meta.insert("destination_code".to_string(), serde_json::Value::String(dst_ring.code.clone()));
                    if let Some(ref action) = dst_ring.action {
                        meta.insert("action".to_string(), serde_json::Value::String(action.clone()));
                    }
                    acts.push(Action::Fairy(Box::new(FairyAction {
                        kind: "fairy_ring",
                        from: FairyFrom {
                            min: [x1, y1, p1],
                            max: [x1, y1, p1],
                            object_id: src_ring.object_id,
                        },
                        to: MinMax::point(x2, y2, p2),
                        code: dst_ring.code.clone(),
                        cost_ms: dst_ring.cost_ms as f64,
                        metadata: serde_json::Value::Object(meta),
                    })));
                } else {
                    // Fallback: unknown edge kind; emit as generic teleport with zero cost
                    acts.push(Action::Teleport(TeleportAction {
                        kind: "teleport",
                        from: MinMax::point(x1, y1, p1),
                        to: MinMax::point(x2, y2, p2),
                        cost_ms: 0,
                    }));
                }
            }
        }
    }

    // If we had a virtual start, add a synthetic first action derived from the selected global teleport metadata/cost.
    if let Some(mut virtual_action) = virtual_start_action {
        if let Some(entry_id) = virtual_entry {
            if let Some(gc) = global_cost.get(&entry_id).copied() {
                virtual_action.cost_ms = serde_json::Number::from_f64(gc as f64)
                    .unwrap_or_else(|| serde_json::Number::from(0));
            }
            if let Some(meta) = global_meta.get(&entry_id) {
                virtual_action.kind = global_step_kind(meta).to_string();
                if let Some(obj) = virtual_action.metadata.as_object_mut() {
                    // The metadata subtree is mutated here, so this one clones out of
                    // the Arc (exactly as before).
                    obj.insert("teleport".to_string(), (**meta).clone());
                }
            }
        }
        acts.insert(0, Action::VirtualStart(Box::new(virtual_action)));
    }

    // The tile the character stands on before the first action. For a virtual start the
    // synthetic teleport action already occupies index 0 and carries it, but on the normal
    // path nothing in `acts` records it — see `optimize_with_surge_dive`.
    let route_origin = res.path.first().map(|&id| coord(id));

    // Apply surge/dive optimization to the actions
    (Some(optimize_with_surge_dive(acts, surge, dive, route_origin)), geometry)
}

/// Everything the blocking task computes for one request; carried back to the handler
/// for the response, metrics, and the log line.
struct RouteTaskOut {
    res: navpath_core::SearchResult,
    virtual_entry: Option<u32>,
    actions: Option<Box<serde_json::value::RawValue>>,
    geometry: Option<Box<serde_json::value::RawValue>>,
    retried: bool,
    attempts_pops: [u32; 3],
    seed_dropped: bool,
    engine: &'static str,
    search_us: u64,
    payload_us: u64,
}

/// Process-lifetime service counters (see [`crate::Metrics`]) plus the live route-cache
/// state and the policy in force — so a zero hit rate can be diagnosed from one call:
/// `cache_miss_seed` is how many requests `NAVPATH_CACHE_IGNORE_SEED=1` would convert
/// into hits, `cache_miss_cold` is how many no cache policy can help.
pub async fn stats(State(state): State<AppState>) -> Json<serde_json::Value> {
    let cur = state.current.load();
    let mut out = state.metrics.snapshot_json();
    let entries = cur
        .route_cache
        .as_ref()
        .and_then(|c| c.lock().ok().map(|c| c.len()))
        .unwrap_or(0);
    let capacity = cur
        .route_cache
        .as_ref()
        .and_then(|c| c.lock().ok().map(|c| c.cap().get()))
        .unwrap_or(0);
    if let Some(obj) = out.as_object_mut() {
        obj.insert(
            "route_cache".to_string(),
            serde_json::json!({
                "enabled": cur.route_cache.is_some(),
                "entries": entries,
                "capacity": capacity,
                "ignore_seed": cache_ignore_seed(),
            }),
        );
        obj.insert("subpath_cache".to_string(), serde_json::json!({
            "enabled": cur.subpath_cache.is_some(),
            "paths_per_profile": crate::subpath_cache_paths(),
        }));
        obj.insert("seeding_disabled".to_string(), serde_json::json!(seeding_disabled()));
        obj.insert("race_enabled".to_string(), serde_json::json!(race_enabled()));
        obj.insert("ready".to_string(), serde_json::json!(state.ready.load(std::sync::atomic::Ordering::Acquire)));
    }
    Json(out)
}

/// Cache seed policy (`NAVPATH_CACHE_IGNORE_SEED`, **default ON since 2026-08-06** —
/// plan v3 §3a): drop the seed from the route-cache key, so repeat traffic with
/// varying seeds — the dominant production shape, which otherwise never hits — is
/// served the cached path. Cached hits lose per-seed tie variety (jitter is
/// < 0.1 ms/edge against 300 ms edges, so only equal-cost tie selection changes —
/// the same trade the budget retry already makes). Measured on the gate that
/// roadmap 5.2 demanded (2026-07-31): 11 of 12 repeat requests became hits,
/// ~118 ms → ~0.3–0.9 ms. Set `NAVPATH_CACHE_IGNORE_SEED=0` to restore the legacy
/// per-seed keying.
/// `NAVPATH_RACE=1`: hedged engine race (uni and bidir concurrently, first stable result
/// wins, loser cancelled). Default off. Costs a second search permit per cache miss
/// while both engines run; degrades to the single-engine path when none is free.
pub fn race_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        matches!(std::env::var("NAVPATH_RACE").ok().as_deref().map(str::trim), Some("1") | Some("true"))
    })
}

fn cache_ignore_seed() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(std::env::var("NAVPATH_CACHE_IGNORE_SEED").ok().as_deref().map(str::trim), Some("0") | Some("false"))
    })
}

/// Server-side seed kill switch (`--no-seed` / `NAVPATH_IGNORE_SEED=1`, default off —
/// plan v3 §3b): every request is treated as unseeded even when the client sends a
/// seed. The seed is cleared at ingestion, so everything downstream — search engine,
/// retry ladder, cache keys, miss attribution — sees an unseeded request: no edge
/// jitter, canonical pruning engages, the seeded retry rungs never run. Responses to
/// requests that DID send a seed carry `degraded: "seed_ignored"` (contract rewrites
/// must be visible — same rule as `seed_dropped`).
pub fn seeding_disabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        matches!(std::env::var("NAVPATH_IGNORE_SEED").ok().as_deref().map(str::trim), Some("1") | Some("true"))
    })
}

pub async fn route(State(state): State<AppState>, Json(mut req): Json<RouteRequest>) -> Result<Json<RouteResponse>, (StatusCode, String)> {
    let start = std::time::Instant::now();
    // Seed kill switch: clear before ANY reader (cache key, shadow attribution,
    // search) so the request is unseeded everywhere, not just in the engine.
    let seed_ignored = seeding_disabled() && req.seed.take().is_some();
    let metrics = state.metrics.clone();
    metrics.requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if !state.ready.load(std::sync::atomic::Ordering::Acquire) {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "warming up (snapshot populate / context pre-warm); retry".into()));
    }
    let cur = state.current.load();
    let Some(snap) = cur.snapshot.as_ref() else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "snapshot not loaded".into()));
    };
    let Some(neighbors) = cur.neighbors.as_ref() else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "neighbors not loaded".into()));
    };
    let globals = cur.globals.clone();
    let counts = snap.counts();

    // Resolve node ids
    let (sid, gid, used_virtual_start) = match (req.start_id, req.goal_id, req.start.as_ref(), req.goal.as_ref()) {
        (Some(sid), Some(gid), _, _) => (sid, gid, false),
        (_, _, Some(s), Some(g)) => {
            // Coordinate lookup is a binary search over the snapshot's packed-coords
            // section (node ids are assigned in packed-key order) — no heap index.
            let Some(gid) = snap.find_node(g.wx, g.wy, g.plane) else { return Err((StatusCode::BAD_REQUEST, "goal tile not found in snapshot".into())); };

            // If the start coordinate isn't present in the snapshot, do NOT snap to a nearby tile.
            // Treat it as out-of-graph and force entry via a global teleport.
            let (sid, used_virtual_start) = if let Some(sid) = snap.find_node(s.wx, s.wy, s.plane) {
                (sid, false)
            } else {
                info!(
                    start_x = s.wx, start_y = s.wy, start_plane = s.plane,
                    "start coordinate not found in snapshot; will force global teleport entry"
                );
                (0, true)
            };
            (sid, gid, used_virtual_start)
        }
        _ => {
            return Err((StatusCode::BAD_REQUEST, "missing start/goal; provide start_id/goal_id or start/goal with {wx,wy,plane}".into()));
        }
    };
    if (!used_virtual_start && sid >= counts.nodes) || gid >= counts.nodes {
        return Err((StatusCode::BAD_REQUEST, "start_id/goal_id out of range".into()));
    }

    let quick_tele = req_has_quick_tele(&req.profile.requirements);

    let mask = build_mask_from_u32(
        snap.req_tags(),
        req.profile.requirements.iter().filter_map(|kv| {
            let (k, v) = (&kv.key, &kv.value);
            if let Some(n) = v.as_i64() {
                Some((k.as_str(), ClientValue::Num(n)))
            } else if let Some(u) = v.as_u64() {
                Some((k.as_str(), ClientValue::Num(u as i64)))
            } else if let Some(b) = v.as_bool() {
                Some((k.as_str(), ClientValue::Num(if b { 1 } else { 0 })))
            } else if let Some(s) = v.as_str() {
                let st = s.trim();
                if let Ok(n) = st.parse::<i64>() {
                    Some((k.as_str(), ClientValue::Num(n)))
                } else {
                    Some((k.as_str(), ClientValue::Str(st)))
                }
            } else {
                None
            }
        }),
    );
    
    // Route results are pure functions of (snapshot, endpoints, eligibility, seed);
    // repeated requests hit the per-snapshot LRU and skip the search entirely (payload
    // is still rebuilt per request so one entry serves every options combination).
    let cache_key = crate::RouteCacheKey {
        virtual_start: used_virtual_start,
        sid: if used_virtual_start { 0 } else { sid },
        gid,
        mask_bits: crate::pack_mask_bits(&mask.satisfied),
        quick_tele,
        seed: if cache_ignore_seed() { None } else { req.seed },
    };
    let cached: Option<crate::RouteCacheEntry> = cur
        .route_cache
        .as_ref()
        .and_then(|c| c.lock().ok().and_then(|mut c| c.get(&cache_key).cloned()));

    // Exact sub-path reuse (crate::SubpathCache): both endpoints on a cached optimal
    // path for this profile => serve the slice, no search. Virtual starts never qualify.
    let profile_key: crate::ProfileKey = (cache_key.mask_bits.clone(), quick_tele);
    let mut goal_known = false;
    // Cached slices carry UNSEEDED (base-cost) `path_g`; a seeded request may use them
    // only under the seed-blind cache policy, exactly like the route cache.
    let subpath_hit: Option<crate::RouteCacheEntry> = if cached.is_none() && !used_virtual_start && (req.seed.is_none() || cache_ignore_seed()) {
        cur.subpath_cache.as_ref().and_then(|c| {
            let (hit, known) = crate::subpath_lookup(c, &profile_key, sid, gid);
            goal_known = known;
            hit.map(|res| Arc::new((res, None, false)))
        })
    } else {
        None
    };
    let subpath_served = subpath_hit.is_some();
    let cached = cached.or(subpath_hit);

    // Attribute the miss (see crate::SeedShadow). A seeded request whose seed-blind key
    // is already known missed *because of the seed*; anything else is a genuinely new
    // (endpoints, profile) pair. The key is rebuilt only where it is used (a seeded miss,
    // or a cache put) so hits and unseeded traffic never pay for the clone.
    let seed_blind_key = || crate::RouteCacheKey { seed: None, ..cache_key.clone() };
    let cache_outcome = if subpath_served {
        crate::CacheOutcome::Subpath
    } else if cur.route_cache.is_none() {
        crate::CacheOutcome::Disabled
    } else if cached.is_some() {
        crate::CacheOutcome::Hit
    } else if cache_key.seed.is_some()
        && cur
            .seed_shadow
            .as_ref()
            .and_then(|s| s.lock().ok().map(|mut s| s.get(&seed_blind_key()).is_some()))
            .unwrap_or(false)
    {
        crate::CacheOutcome::MissSeed
    } else {
        crate::CacheOutcome::MissCold
    };
    {
        use std::sync::atomic::Ordering::Relaxed;
        match cache_outcome {
            crate::CacheOutcome::MissSeed => { metrics.cache_miss_seed.fetch_add(1, Relaxed); }
            crate::CacheOutcome::MissCold => { metrics.cache_miss_cold.fetch_add(1, Relaxed); }
            _ => {}
        }
        if cached.is_none() && goal_known {
            metrics.cache_miss_goal_known.fetch_add(1, Relaxed);
        }
    }

    // Exact reachability precheck (roadmap 4.1): eligibility never gates walk edges,
    // so "can this goal be reached at all under this profile" is decided on the
    // ~491-component condensation in microseconds — BEFORE a permit, a blocking
    // thread, or a context pair is committed. Every rejection here is a budget-capped
    // ~1.5M-pop flood (plus its retry) that never ran. The verdict is exact, so the
    // response is identical to what the flood would have produced.
    if cached.is_none() {
        if let Some(cg) = cur.comp_graph.as_ref() {
            let comps = snap.comp_ids();
            let start_comp = if used_virtual_start { None } else { Some(comps[sid as usize]) };
            let goal_comp = comps[gid as usize];
            if !engine_adapter::goal_reachable(cg, &mask, start_comp, goal_comp) {
                use std::sync::atomic::Ordering::Relaxed;
                metrics.precheck_rejects.fetch_add(1, Relaxed);
                metrics.not_found.fetch_add(1, Relaxed);
                let duration_us = start.elapsed().as_micros() as u64;
                let duration_ms = (duration_us / 1000) as u128;
                info!(duration_ms, sid, gid, virtual_start = used_virtual_start,
                      "route rejected by component reachability precheck");
                return Ok(Json(RouteResponse {
                    found: false,
                    cost: f32::INFINITY,
                    path: Vec::new(),
                    length_tiles: 0,
                    duration_ms,
                    duration_us,
                    reason: None,
                    degraded: None,
                    actions: None,
                    geometry: None,
                }));
            }
        }
    }

    // Offload search to a blocking thread, bounded by the search semaphore so a burst of
    // slow queries cannot pin hundreds of blocking-pool threads (each holding a
    // node-sized SearchContext). Overload fails fast instead of queueing floods. Cache
    // hits skip the search and need no permit. The permit moves into the blocking task
    // and is released the moment the search itself finishes — payload building and
    // response serialization must not count against search admission.
    let permit = if cached.is_none() {
        match state.search_permits.clone().try_acquire_owned() {
            Ok(p) => Some(p),
            Err(_) => {
                metrics.semaphore_rejects.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                warn!(sid, gid, "search capacity exhausted; rejecting with 503");
                return Err((StatusCode::SERVICE_UNAVAILABLE, "search capacity exhausted; retry".into()));
            }
        }
    } else {
        None
    };

    // Per-profile search artifacts (roadmap 5.4): MacroFilters, eligible globals and
    // fairy sets are pure functions of (snapshot, exact mask bits, quick_tele), so
    // cache-missing requests resolve them from the per-snapshot LRU instead of
    // rebuilding. Cheap (a lock + at worst one ~1k-slot scan), so it runs here before
    // the blocking task; cache hits skip it entirely.
    let artifacts: Option<Arc<engine_adapter::ProfileArtifacts>> = if cached.is_none() {
        let key: crate::ProfileKey = profile_key.clone();
        let hit = cur
            .profile_cache
            .lock()
            .ok()
            .and_then(|mut c| c.get(&key).cloned());
        Some(match hit {
            Some(a) => a,
            None => {
                let built = Arc::new(engine_adapter::build_profile_artifacts(
                    neighbors.as_ref(),
                    cur.neighbors_rev.as_deref(),
                    globals.as_slice(),
                    cur.fairy_rings.as_slice(),
                    &mask,
                    quick_tele,
                ));
                if let Ok(mut c) = cur.profile_cache.lock() {
                    c.put(key, built.clone());
                }
                built
            }
        })
    } else {
        None
    };

    let snap_arc = snap.clone();
    let neighbors_arc = neighbors.clone();
    let neighbors_rev_arc = cur.neighbors_rev.clone();
    let globals_arc = globals.clone();
    let fairy_rings_arc = cur.fairy_rings.clone();
    let node_to_fairy_ring_arc = cur.node_to_fairy_ring.clone();
    let seed = req.seed;
    let mask_for_search = mask.clone();
    let mask_for_payload = mask.clone();

    // Hedged race (`NAVPATH_RACE=1`): run the uni and bidir engines concurrently and
    // serve the first stable result. Both engines are exact, so the served cost is
    // identical either way; what the race buys is the per-pair minimum of two engines
    // whose relative speed swings 3-5x in both directions depending on whether the
    // route is walk- or teleport-dominated (docs/route_latency_improvements_2026-09-17.md
    // §1.2/§2.1). The second engine needs its own search permit: when none is free the
    // request silently degrades to the single-engine policy path, so the race never
    // adds admission pressure under load.
    let mut permit = permit;
    let race_permit = if cached.is_none()
        && race_enabled()
        && engine_adapter::bidir_enabled()
        && neighbors_rev_arc.is_some()
    {
        state.search_permits.clone().try_acquire_owned().ok()
    } else {
        None
    };
    let race = race_permit.is_some();

    // Cooperative cancellation: one flag per engine (the race cancels only the loser);
    // the disconnect guard and the route deadline flip every flag. The engine checks
    // its flag every 1024 pops, and the retry ladder never starts a rung once it is set.
    let cancel_flags: Vec<Arc<std::sync::atomic::AtomicBool>> =
        (0..if race { 2 } else { 1 }).map(|_| Arc::new(std::sync::atomic::AtomicBool::new(false))).collect();
    struct CancelOnDrop(Vec<Arc<std::sync::atomic::AtomicBool>>, bool);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            if !self.1 {
                for f in &self.0 {
                    f.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }
    let mut disconnect_guard = CancelOnDrop(cancel_flags.clone(), false);

    let used_virtual_start_for_search = used_virtual_start;
    let macro_lookup_arc = cur.macro_lookup.clone();
    let req_tag_index_arc = cur.req_tag_index.clone();
    let return_geometry = req.options.return_geometry;
    let only_actions = req.options.only_actions;
    let surge_cfg = req.surge.clone();
    let dive_cfg = req.dive.clone();
    let virtual_start_from = if used_virtual_start {
        req.start.as_ref().map(|c| (c.wx, c.wy, c.plane))
    } else {
        None
    };
    let cached_for_task = cached.clone();
    let canonical_for_search = cur.canonical_grid.clone();
    let ctx_pool = state.ctx_pool.clone();

    // One fresh search on `engine`, observing `cancel`. Cloneable (captures are Arcs
    // and Copy values) so the race can hand one copy to each blocking task.
    let snap_for_search = snap_arc.clone();
    let run_search = move |engine: engine_adapter::EngineChoice,
                           cancel: Arc<std::sync::atomic::AtomicBool>|
     -> (engine_adapter::SearchOutcome, Option<u32>) {
        let arts = artifacts.as_ref().expect("profile artifacts resolved for fresh searches");
        // Checkout scope: the pair returns to the pool when this closure returns,
        // before payload building.
        let mut pooled = ctx_pool.checkout();
        if used_virtual_start_for_search {
            engine_adapter::run_route_with_requirements_virtual_start(
                snap_for_search.clone(),
                neighbors_arc.clone(),
                neighbors_rev_arc.clone(),
                gid,
                seed,
                Some(cancel.as_ref()),
                arts,
                canonical_for_search.clone(),
                engine,
                pooled.pair(),
            )
        } else {
            (
                engine_adapter::run_route_with_requirements_and_fairy_rings(
                    snap_for_search.clone(),
                    neighbors_arc.clone(),
                    neighbors_rev_arc.clone(),
                    sid,
                    gid,
                    &mask_for_search,
                    seed,
                    Some(cancel.as_ref()),
                    arts,
                    canonical_for_search.clone(),
                    engine,
                    pooled.pair(),
                ),
                None,
            )
        }
    };

    // Payload build + serialization for one search outcome, off the reactor.
    let build_payload = move |outcome: engine_adapter::SearchOutcome, virtual_entry: Option<u32>, search_us: u64| -> RouteTaskOut {
        let t_payload = std::time::Instant::now();
        let (actions, geometry) = build_route_payload(
            &snap_arc,
            &globals_arc,
            &macro_lookup_arc,
            &fairy_rings_arc,
            &node_to_fairy_ring_arc,
            &req_tag_index_arc,
            &mask_for_payload,
            quick_tele,
            return_geometry,
            only_actions,
            &surge_cfg,
            &dive_cfg,
            virtual_start_from,
            virtual_entry,
            sid,
            &outcome.res,
        );
        // Serialize the bulky payload halves here, off the reactor; `payload_us`
        // deliberately includes it (it is payload work).
        let actions = actions
            .map(|a| serde_json::value::to_raw_value(&a).expect("actions serialize"));
        let geometry = geometry
            .map(|g| serde_json::value::to_raw_value(&g).expect("geometry serialize"));
        let payload_us = t_payload.elapsed().as_micros() as u64;
        RouteTaskOut {
            res: outcome.res,
            virtual_entry,
            actions,
            geometry,
            retried: outcome.retried,
            attempts_pops: outcome.attempts_pops,
            seed_dropped: outcome.seed_dropped,
            engine: outcome.engine,
            search_us,
            payload_us,
        }
    };

    let work: std::pin::Pin<Box<dyn std::future::Future<Output = Result<RouteTaskOut, String>> + Send>> = if !race {
        let cancel = cancel_flags[0].clone();
        let permit = permit.take();
        let join = tokio::task::spawn_blocking(move || {
            let t_search = std::time::Instant::now();
            let (outcome, virtual_entry) = if let Some(hit) = cached_for_task {
                (
                    engine_adapter::SearchOutcome { res: hit.0.clone(), retried: false, attempts_pops: [0, 0, 0], seed_dropped: hit.2, engine: "cache" },
                    hit.1,
                )
            } else {
                run_search(engine_adapter::EngineChoice::Policy, cancel)
            };
            let search_us = t_search.elapsed().as_micros() as u64;
            drop(permit);
            build_payload(outcome, virtual_entry, search_us)
        });
        Box::pin(async move { join.await.map_err(|e| e.to_string()) })
    } else {
        // Each racer runs its search, and the first one to finish with a STABLE result
        // (Found / genuine NotFound) claims the request: it cancels the other engine and
        // builds the payload on its own thread, so the winning path costs no extra task
        // hop. A truncated result (budget/cancel) never claims; if both truncate, the
        // handler picks the better one and builds the payload itself.
        enum RaceMsg {
            Done(RouteTaskOut),
            Truncated(engine_adapter::SearchOutcome, Option<u32>, u64),
        }
        fn stable(o: &engine_adapter::SearchOutcome) -> bool {
            matches!(o.res.status, navpath_core::SearchStatus::Found | navpath_core::SearchStatus::NotFound)
        }
        let claimed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<RaceMsg>(2);
        let racers = [
            (engine_adapter::EngineChoice::Uni, 0usize, permit.take()),
            (engine_adapter::EngineChoice::Bidir, 1usize, race_permit),
        ];
        for (engine, idx, permit) in racers {
            let run = run_search.clone();
            let payload = build_payload.clone();
            let tx = tx.clone();
            let claimed = claimed.clone();
            let flags = cancel_flags.clone();
            tokio::task::spawn_blocking(move || {
                let t_search = std::time::Instant::now();
                let (outcome, virtual_entry) = run(engine, flags[idx].clone());
                let search_us = t_search.elapsed().as_micros() as u64;
                drop(permit);
                let won = stable(&outcome)
                    && claimed
                        .compare_exchange(false, true, std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Acquire)
                        .is_ok();
                if won {
                    flags[1 - idx].store(true, std::sync::atomic::Ordering::Relaxed);
                    let _ = tx.blocking_send(RaceMsg::Done(payload(outcome, virtual_entry, search_us)));
                } else {
                    // Loser (or truncated): the receiver may already be gone.
                    let _ = tx.blocking_send(RaceMsg::Truncated(outcome, virtual_entry, search_us));
                }
            });
        }
        drop(tx);
        let metrics = metrics.clone();
        Box::pin(async move {
            metrics.race_runs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut truncated: Option<(engine_adapter::SearchOutcome, Option<u32>, u64)> = None;
            loop {
                match rx.recv().await {
                    Some(RaceMsg::Done(out)) => {
                        let ctr = if out.engine == "uni" { &metrics.race_wins_uni } else { &metrics.race_wins_bidir };
                        ctr.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        return Ok(out);
                    }
                    Some(RaceMsg::Truncated(o, ve, ms)) => {
                        // A stable loser also lands here (it lost the claim); the
                        // winner's Done is already in the channel or arriving, so keep
                        // waiting unless this is the second message.
                        truncated = Some(match truncated.take() {
                            None => (o, ve, ms),
                            Some((po, pve, pms)) => {
                                if stable(&o) || (o.res.found && (!po.res.found || o.res.cost < po.res.cost)) { (o, ve, ms) } else { (po, pve, pms) }
                            }
                        });
                    }
                    None => break,
                }
            }
            let (o, ve, ms) = truncated.ok_or_else(|| "race: no engine reported".to_string())?;
            tokio::task::spawn_blocking(move || build_payload(o, ve, ms)).await.map_err(|e| e.to_string())
        })
    };
    let out = match tokio::time::timeout(route_deadline(), work).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e)),
        Err(_) => {
            for f in &cancel_flags {
                f.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            metrics.deadline_timeouts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            warn!(sid, gid, deadline_ms = route_deadline().as_millis() as u64, "route deadline exceeded; returning 504");
            return Err((StatusCode::GATEWAY_TIMEOUT, "route deadline exceeded".into()));
        }
    };
    // Search finished; disarm the disconnect guard so late drops don't poison anything.
    disconnect_guard.1 = true;

    let RouteTaskOut { mut res, virtual_entry, actions, geometry, retried, attempts_pops, seed_dropped, engine, search_us, payload_us } = out;

    // Populate the cache on fresh, stable outcomes (Found / genuine NotFound only —
    // budget or cancellation truncations, including truncated-found results whose cost
    // is unproven, are transient and must not stick).
    if cached.is_none() {
        // Only unseeded results: their `path_g` are base costs valid for every request.
        if !used_virtual_start && seed.is_none() {
            if let Some(c) = cur.subpath_cache.as_ref() {
                crate::subpath_insert(c, profile_key.clone(), &res);
            }
        }
        if matches!(res.status, navpath_core::SearchStatus::Found | navpath_core::SearchStatus::NotFound) {
            if let Some(c) = cur.route_cache.as_ref() {
                // Built before the key is moved into the cache. Keeping the attribution
                // index in step with what the cache actually holds is what stops
                // `miss_seed` from claiming a hit the policy could not have delivered.
                let shadow = seed_blind_key();
                if let Ok(mut c) = c.lock() {
                    c.put(cache_key, Arc::new((res.clone(), virtual_entry, seed_dropped)));
                    metrics.cache_puts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                if let Some(s) = cur.seed_shadow.as_ref() {
                    if let Ok(mut s) = s.lock() {
                        s.put(shadow, ());
                    }
                }
            }
        }
    }

    {
        use std::sync::atomic::Ordering::Relaxed;
        let cache_hit = cached.is_some();
        if subpath_served {
            metrics.cache_subpath_hits.fetch_add(1, Relaxed);
        } else if cache_hit {
            metrics.cache_hits.fetch_add(1, Relaxed);
        } else {
            metrics.searches.fetch_add(1, Relaxed);
            if retried {
                metrics.retries.fetch_add(1, Relaxed);
                if res.found {
                    metrics.retry_found.fetch_add(1, Relaxed);
                }
            }
            metrics.record_pops(res.pops as u64);
            metrics.record_search_ms(search_us / 1000);
            metrics.record_search_us(search_us, res.pops as u64);
        }
        match res.status {
            navpath_core::SearchStatus::Found => metrics.found.fetch_add(1, Relaxed),
            navpath_core::SearchStatus::NotFound => metrics.not_found.fetch_add(1, Relaxed),
            navpath_core::SearchStatus::BudgetExceeded => metrics.budget_exceeded.fetch_add(1, Relaxed),
            navpath_core::SearchStatus::Cancelled => metrics.cancelled.fetch_add(1, Relaxed),
        };
    }

    let duration_us = start.elapsed().as_micros() as u64;
    let duration_ms = (duration_us / 1000) as u128;
    let length_tiles = res.path.len();
    // only_actions means exactly that: skip the duplicate node-id path in the payload.
    let path = if only_actions { Vec::new() } else { std::mem::take(&mut res.path) };

    let reason = match res.status {
        navpath_core::SearchStatus::BudgetExceeded => Some("budget_exceeded".to_string()),
        navpath_core::SearchStatus::Cancelled => Some("cancelled".to_string()),
        _ => None,
    };
    let degraded = if seed_dropped {
        Some("seed_dropped".to_string())
    } else if seed_ignored {
        // The client sent a seed but the server runs with --no-seed: the served route
        // is the deterministic unseeded optimum.
        Some("seed_ignored".to_string())
    } else {
        None
    };
    let resp = RouteResponse {
        found: res.found,
        cost: res.cost,
        path,
        length_tiles,
        duration_ms,
        duration_us,
        reason,
        degraded,
        actions,
        geometry,
    };
    if let Some(dump_path) = result_dump_path() {
        if let Ok(bytes) = serde_json::to_vec_pretty(&resp) {
            let _ = std::fs::write(dump_path, bytes);
        }
    }
    info!(
        duration_ms = duration_ms,
        search_ms = search_us / 1000,
        payload_ms = payload_us / 1000,
        duration_us = duration_us,
        search_us = search_us,
        payload_us = payload_us,
        // Memory-behaviour signal: ~150-230 warm, tens of thousands on a cold page cache.
        ns_per_pop = if res.pops > 0 { search_us * 1000 / res.pops as u64 } else { 0 },
        found = res.found,
        cost = res.cost,
        length = length_tiles,
        status = ?res.status,
        pops = res.pops,
        pops_f = res.pops_f,
        pops_b = res.pops_b,
        retried = retried,
        first_attempt_pops = attempts_pops[0],
        retry_pops = attempts_pops[1],
        retry_unseeded_pops = attempts_pops[2],
        seed_dropped = seed_dropped,
        cache_hit = cached.is_some(),
        // Which engine served it: uni | bidir | cache (race winners are uni/bidir).
        engine = engine,
        // Why the cache did/didn't serve this: hit | miss_seed | miss_cold | off.
        cache = cache_outcome.as_str(),
        "route request completed"
    );
    Ok(Json(resp))
}

#[derive(Debug, Serialize)]
pub struct ReloadResponse { pub reloaded: bool, pub snapshot_hash: Option<String>, pub loaded_at: u64 }

pub async fn reload(State(state): State<AppState>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let cur = state.current.load();
    let path = cur.path.clone();

    // Snapshot open + provider/component/canonical builds are ~100 ms of CPU work —
    // run them on the blocking pool so reactor threads keep serving requests.
    let log_path = path.clone();
    let built = tokio::task::spawn_blocking(move || {
        let new_snap = navpath_core::Snapshot::open(&path).map_err(|e| e.to_string())?;
        // Page the new mapping in BEFORE it is swapped live.
        crate::warm_snapshot(&new_snap);
        let new_hash = crate::read_tail_hash_hex(&path);
        // Pre-compute neighbors and globals
        let (neighbors, neighbors_rev, globals, macro_lookup) = crate::engine_adapter::build_neighbor_provider(&new_snap);
        // Pre-compute fairy rings
        let (fairy_rings, node_to_fairy_ring) = crate::engine_adapter::build_fairy_rings(&new_snap);
        let comp_graph = crate::engine_adapter::build_component_graph(&new_snap, &globals, &fairy_rings);
        let canonical_grid = crate::engine_adapter::build_canonical_grid(&new_snap);
        let req_tag_index = crate::build_req_tag_index(Some(&new_snap));
        Ok::<SnapshotState, String>(SnapshotState {
            path,
            snapshot: Some(Arc::new(new_snap)),
            neighbors: Some(Arc::new(neighbors)),
            neighbors_rev: Some(Arc::new(neighbors_rev)),
            globals: Arc::new(globals),
            macro_lookup: Arc::new(macro_lookup),
            req_tag_index: Arc::new(req_tag_index),
            loaded_at_unix: crate::now_unix(),
            snapshot_hash_hex: new_hash,
            route_cache: crate::new_route_cache(),
            seed_shadow: crate::new_seed_shadow(),
            fairy_rings: Arc::new(fairy_rings),
            node_to_fairy_ring: Arc::new(node_to_fairy_ring),
            comp_graph: Some(Arc::new(comp_graph)),
            canonical_grid,
            profile_cache: crate::new_profile_cache(), subpath_cache: crate::new_subpath_cache(),
        })
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    match built {
        Ok(new_state) => {
            let new_hash = new_state.snapshot_hash_hex.clone();
            state.current.store(Arc::new(new_state));
            info!(path=?log_path, hash=?new_hash, "reloaded snapshot");
            let latest = state.current.load();
            Ok(Json(serde_json::json!({
                "reloaded": true,
                "snapshot_hash": latest.snapshot_hash_hex,
                "loaded_at": latest.loaded_at_unix
            })))
        }
        Err(e) => {
            warn!(error=%e, path=?log_path, "reload failed; keeping old snapshot");
            Err((StatusCode::CONFLICT, e))
        }
    }
}

#[cfg(test)]
mod surge_dive_tests {
    use super::*;

    fn mv(x: i32, y: i32) -> Action {
        Action::Move(MoveAction { kind: "move", to: [x, y, 0], cost_ms: 600.0 })
    }

    fn cfgs() -> (SurgeConfig, DiveConfig) {
        (
            SurgeConfig { enabled: true, charges: 2, cooldown_ms: 20400.0 },
            DiveConfig { enabled: true, available_in_ms: 0.0, cooldown_ms: 20400.0 },
        )
    }

    fn abilities(actions: &[Action]) -> Vec<(&'static str, [i32; 3], [i32; 3])> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Ability(ab) => Some((ab.kind, ab.from, ab.to)),
                _ => None,
            })
            .collect()
    }

    /// Every tile the input walked must still be reachable from the output: abilities
    /// replace a run of moves, so walking each ability's `from -> to` plus every emitted
    /// move must land on exactly the final input tile, having covered every input tile.
    fn covered_tiles(out: &[Action], origin: (i32, i32, i32)) -> Vec<[i32; 3]> {
        let mut seen = vec![[origin.0, origin.1, origin.2]];
        for a in out {
            if let Action::Ability(ab) = a {
                assert_eq!(ab.from, *seen.last().unwrap(), "ability starts where the character is");
            }
            let (x, y, p) = a.to_coords();
            seen.push([x, y, p]);
        }
        seen
    }

    /// Dive east then surge east: the dive establishes facing, so the 3-walk rule is waived.
    #[test]
    fn same_direction_dive_waives_walk_requirement() {
        let path: Vec<Action> = (1..=20).map(|x| mv(x, 0)).collect();
        let (s, d) = cfgs();
        let out = optimize_with_surge_dive(path, &s, &d, Some((0, 0, 0)));
        let abs = abilities(&out);
        println!("same-direction: {:?}", abs);
        assert_eq!(abs.len(), 2, "expected a dive followed immediately by a surge");
        assert_eq!(abs[0].0, "dive");
        assert_eq!(abs[1].0, "surge");
        // Surge must start exactly where the dive landed (no walking in between).
        assert_eq!(abs[1].1, abs[0].2);
    }

    /// Dive north then a north-east surge: the dive leaves the wrong facing, so the
    /// walk requirement still applies and the surge cannot fire off the dive.
    #[test]
    fn turning_dive_still_requires_walk() {
        let mut path: Vec<Action> = (1..=11).map(|y| mv(0, y)).collect();
        path.extend((1..=15).map(|x| mv(x, 11)));
        let (s, d) = cfgs();
        let out = optimize_with_surge_dive(path, &s, &d, Some((0, 0, 0)));
        let abs = abilities(&out);
        println!("turning: {:?}", abs);
        assert_eq!(abs[0].0, "dive");
        if let Some(surge) = abs.iter().find(|a| a.0 == "surge") {
            assert_ne!(surge.1, abs[0].2, "surge must not fire straight off a turning dive");
        }
    }

    /// Regression: the leading ability must fire from the character's ACTUAL tile and
    /// must not swallow the opening walk step. Before the origin was threaded through,
    /// the dive reported `from = (1,0)` (the first move's destination), covered only 9
    /// tiles while claiming 10, and the step (0,0)->(1,0) vanished from the payload.
    #[test]
    fn leading_ability_starts_at_the_route_origin() {
        let path: Vec<Action> = (1..=20).map(|x| mv(x, 0)).collect();
        let (s, d) = cfgs();
        let out = optimize_with_surge_dive(path, &s, &d, Some((0, 0, 0)));
        let abs = abilities(&out);
        assert_eq!(abs[0].1, [0, 0, 0], "dive must start on the character's tile");
        // 10 tiles covered from (0,0) lands on (10,0) — the reported span and the
        // advertised tile count now agree.
        assert_eq!(abs[0].2, [10, 0, 0]);
        let Action::Ability(ab) = &out[0] else { panic!("first action is the dive") };
        assert_eq!(ab.tiles_covered, 10);
        // No tile is lost: the walk ends where the last input move ended.
        let seen = covered_tiles(&out, (0, 0, 0));
        assert_eq!(*seen.last().unwrap(), [20, 0, 0]);
    }

    /// Every emitted ability must be within reach. Sizing the leading one from the wrong
    /// origin let an 11-tile hop through, and surge had no range check at all.
    fn assert_all_abilities_in_range(out: &[Action]) {
        for a in out {
            if let Action::Ability(ab) = a {
                let dist = straight_line_distance(ab.from[0], ab.from[1], ab.to[0], ab.to[1]);
                assert!(
                    dist <= MAX_ABILITY_TILES as f64 + 0.5,
                    "{} spans {dist} tiles from {:?} to {:?}",
                    ab.kind, ab.from, ab.to
                );
            }
        }
    }

    #[test]
    fn leading_dive_respects_the_ten_tile_range() {
        let path: Vec<Action> = (1..=20).map(|x| mv(x, 0)).collect();
        let (s, d) = cfgs();
        assert_all_abilities_in_range(&optimize_with_surge_dive(path, &s, &d, Some((0, 0, 0))));
    }

    /// Regression: a run of DIAGONAL steps. Ten diagonal moves displace 10*sqrt(2) =
    /// 14.14 tiles, which surge used to accept (it only tested straightness) while dive
    /// refused the identical endpoints.
    #[test]
    fn diagonal_run_never_emits_an_out_of_range_surge() {
        let path: Vec<Action> = (1..=20).map(|i| mv(i, i)).collect();
        let (s, d) = cfgs();
        let out = optimize_with_surge_dive(path, &s, &d, Some((0, 0, 0)));
        assert_all_abilities_in_range(&out);
        // The route must still be walked in full, ability or not.
        let seen = covered_tiles(&out, (0, 0, 0));
        assert_eq!(*seen.last().unwrap(), [20, 20, 0]);
    }

    /// A virtual start already carries the origin in its synthetic action, so the
    /// fallback path must not double-count or shift it.
    #[test]
    fn virtual_start_action_supplies_the_origin() {
        let mut path: Vec<Action> = vec![Action::VirtualStart(Box::new(VirtualStartAction {
            kind: "lodestone".to_string(),
            from: MinMax::point(-1, -1, 0),
            to: MinMax::point(0, 0, 0),
            cost_ms: serde_json::Number::from(0),
            metadata: serde_json::json!({}),
        }))];
        path.extend((1..=20).map(|x| mv(x, 0)));
        let (s, d) = cfgs();
        let out = optimize_with_surge_dive(path, &s, &d, Some((0, 0, 0)));
        let abs = abilities(&out);
        assert_eq!(abs[0].1, [0, 0, 0], "dive starts where the teleport landed");
        let seen = covered_tiles(&out, (-1, -1, 0));
        assert_eq!(*seen.last().unwrap(), [20, 0, 0]);
    }
}
