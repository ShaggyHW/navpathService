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

fn build_req_id_to_tag_index(req_words: &[u32]) -> std::collections::HashMap<u32, usize> {
    let mut map = std::collections::HashMap::new();
    let mut i = 0usize;
    while i + 3 < req_words.len() {
        map.insert(req_words[i], i / 4);
        i += 4;
    }
    map
}

/// Parse a macro edge's metadata once and return it if the profile satisfies the edge's
/// requirements (missing/unparseable metadata counts as allowed, matching the search's
/// fail-open handling of empty requirement lists). None = edge not allowed.
fn macro_edge_meta_if_allowed(
    snap: &navpath_core::Snapshot,
    macro_idx: usize,
    req_id_to_tag_idx: &std::collections::HashMap<u32, usize>,
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

/// A macro edge step (door/lodestone/npc/object/item/ifslot/teleport).
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

/// Check if a dive path is valid (straight-line distance within range)
/// Dive teleports directly to target, so we only care about straight-line distance
fn is_valid_dive_path(start: (i32, i32, i32), end: (i32, i32, i32), _tiles_walked: usize) -> bool {
    if start.2 != end.2 {
        return false; // Different planes
    }
    let straight_dist = straight_line_distance(start.0, start.1, end.0, end.1);
    // Dive can reach up to 10 tiles in a straight line regardless of walked path
    straight_dist <= (MAX_ABILITY_TILES as f64) + 0.5
}

/// Optimize actions by inserting surge and dive abilities. Consumes the action list:
/// non-move actions are moved (never cloned) into the output, and walked tiles are
/// `Copy` structs — no per-action JSON re-parsing or cloning (roadmap 5.3).
fn optimize_with_surge_dive(
    actions: Vec<Action>,
    surge_config: &SurgeConfig,
    dive_config: &DiveConfig,
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

        // Get the starting position from the previous action (its destination tile).
        let start_pos = result.last().map(Action::to_coords);

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
                // fallback: use first move destination (not ideal but handles edge case)
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
                    if is_valid_dive_path(current_pos, end_pos, dive_count) {
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

                        if end_p != current_pos.2 {
                            continue;
                        }

                        let straight_dist = straight_line_distance(current_pos.0, current_pos.1, end_x, end_y);

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
                                if idx == 0 {
                                    break;
                                }
                                let prev_from = result[idx - 1].to_coords();

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
    /// Present when the search gave up rather than proving its answer
    /// ("budget_exceeded" or "cancelled"). With found=false the goal may still be
    /// reachable; with found=true the returned path is valid but was not proven
    /// optimal (the search was truncated mid-proof). Absent on proven outcomes.
    #[serde(skip_serializing_if = "Option::is_none")] pub reason: Option<String>,
    /// Present when a request-level guarantee was traded for an answer. Currently only
    /// "seed_dropped": the request sent a seed, both seeded attempts exhausted their
    /// budgets, and the served route is the deterministic unseeded optimum.
    #[serde(skip_serializing_if = "Option::is_none")] pub degraded: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub actions: Option<Vec<Action>>,
    #[serde(skip_serializing_if = "Option::is_none")] pub geometry: Option<Vec<[i32; 3]>>,
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

pub async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    let cur = state.current.load();
    let counts = cur.snapshot.as_ref().map(|s| s.counts()).map(|c| Counts {
        nodes: c.nodes,
        walk_edges: c.walk_edges,
        macro_edges: c.macro_edges,
        req_tags: c.req_tags,
        landmarks: c.landmarks,
    });
    let version = cur.snapshot.as_ref().map(|s| s.manifest().version).unwrap_or(0);
    Json(HealthResponse {
        version,
        snapshot_hash: cur.snapshot_hash_hex.clone(),
        loaded_at: cur.loaded_at_unix,
        counts,
    })
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
    macro_lookup: &std::collections::HashMap<(u32, u32), Vec<u32>>,
    fairy_rings: &[engine_adapter::FairyRing],
    node_to_fairy_ring: &std::collections::HashMap<u32, usize>,
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

    let req_id_to_tag_idx = build_req_id_to_tag_index(snap.req_tags());

    // Eligible global teleports for action annotation, from the metadata parsed once
    // at snapshot load (no per-request 113KB JSON re-parse). Metadata stays behind the
    // shared Arc — serialization reads through it, so nothing is deep-cloned here.
    let mut global_cost: std::collections::HashMap<u32, f32> = std::collections::HashMap::new();
    let mut global_meta: std::collections::HashMap<u32, Arc<serde_json::Value>> = std::collections::HashMap::new();
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

    // Apply surge/dive optimization to the actions
    (Some(optimize_with_surge_dive(acts, surge, dive)), geometry)
}

/// Everything the blocking task computes for one request; carried back to the handler
/// for the response, metrics, and the log line.
struct RouteTaskOut {
    res: navpath_core::SearchResult,
    virtual_entry: Option<u32>,
    actions: Option<Vec<Action>>,
    geometry: Option<Vec<[i32; 3]>>,
    retried: bool,
    attempts_pops: [u32; 3],
    seed_dropped: bool,
    search_ms: u64,
    payload_ms: u64,
}

/// Process-lifetime service counters (see [`crate::Metrics`]).
pub async fn stats(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(state.metrics.snapshot_json())
}

/// Opt-in cache policy (`NAVPATH_CACHE_IGNORE_SEED=1`, default off): drop the seed
/// from the route-cache key, so repeat traffic with varying seeds — the dominant
/// production shape, which otherwise never hits — is served the cached path. Cached
/// hits then lose per-seed tie variety (jitter is < 0.1 ms/edge against 300 ms edges,
/// so only equal-cost tie selection changes — the same trade the budget retry already
/// makes). Flip only with the client owner's sign-off; measure hit rates via /stats
/// first (roadmap 5.2).
fn cache_ignore_seed() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        matches!(std::env::var("NAVPATH_CACHE_IGNORE_SEED").ok().as_deref().map(str::trim), Some("1") | Some("true"))
    })
}

pub async fn route(State(state): State<AppState>, Json(req): Json<RouteRequest>) -> Result<Json<RouteResponse>, (StatusCode, String)> {
    let start = std::time::Instant::now();
    let metrics = state.metrics.clone();
    metrics.requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
                let duration_ms = start.elapsed().as_millis();
                info!(duration_ms, sid, gid, virtual_start = used_virtual_start,
                      "route rejected by component reachability precheck");
                return Ok(Json(RouteResponse {
                    found: false,
                    cost: f32::INFINITY,
                    path: Vec::new(),
                    length_tiles: 0,
                    duration_ms,
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
    // hits skip the search and need no permit.
    let _permit = if cached.is_none() {
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
        let key: crate::ProfileKey = (cache_key.mask_bits.clone(), quick_tele);
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

    // Cooperative cancellation: flipped when the client disconnects (the handler future
    // is dropped) or the route deadline fires; the engine checks it every 1024 pops.
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    struct CancelOnDrop(Arc<std::sync::atomic::AtomicBool>, bool);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            if !self.1 {
                self.0.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    let mut disconnect_guard = CancelOnDrop(cancel.clone(), false);

    let used_virtual_start_for_search = used_virtual_start;
    let cancel_for_search = cancel.clone();
    let macro_lookup_arc = cur.macro_lookup.clone();
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
    let join = tokio::task::spawn_blocking(move || {
        let cancel_ref = Some(cancel_for_search.as_ref());
        let t_search = std::time::Instant::now();
        let (outcome, virtual_entry) = if let Some(hit) = cached_for_task {
            (
                engine_adapter::SearchOutcome { res: hit.0.clone(), retried: false, attempts_pops: [0, 0, 0], seed_dropped: hit.2 },
                hit.1,
            )
        } else if used_virtual_start_for_search {
            let arts = artifacts.as_ref().expect("profile artifacts resolved for fresh searches");
            // Checkout scope: the pair returns to the pool before payload building.
            let mut pooled = ctx_pool.checkout();
            engine_adapter::run_route_with_requirements_virtual_start(
                snap_arc.clone(),
                neighbors_arc.clone(),
                neighbors_rev_arc,
                gid,
                seed,
                cancel_ref,
                arts,
                canonical_for_search.clone(),
                pooled.pair(),
            )
        } else {
            let arts = artifacts.as_ref().expect("profile artifacts resolved for fresh searches");
            let mut pooled = ctx_pool.checkout();
            (engine_adapter::run_route_with_requirements_and_fairy_rings(
                snap_arc.clone(), neighbors_arc.clone(), neighbors_rev_arc, sid, gid, &mask_for_search, seed,
                cancel_ref,
                arts,
                canonical_for_search.clone(),
                pooled.pair(),
            ), None)
        };
        let search_ms = t_search.elapsed().as_millis() as u64;
        let t_payload = std::time::Instant::now();
        let (actions, geometry) = build_route_payload(
            &snap_arc,
            &globals_arc,
            &macro_lookup_arc,
            &fairy_rings_arc,
            &node_to_fairy_ring_arc,
            &mask_for_search,
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
        let payload_ms = t_payload.elapsed().as_millis() as u64;
        RouteTaskOut {
            res: outcome.res,
            virtual_entry,
            actions,
            geometry,
            retried: outcome.retried,
            attempts_pops: outcome.attempts_pops,
            seed_dropped: outcome.seed_dropped,
            search_ms,
            payload_ms,
        }
    });
    let out = match tokio::time::timeout(route_deadline(), join).await {
        Ok(joined) => joined.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
        Err(_) => {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            metrics.deadline_timeouts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            warn!(sid, gid, deadline_ms = route_deadline().as_millis() as u64, "route deadline exceeded; returning 504");
            return Err((StatusCode::GATEWAY_TIMEOUT, "route deadline exceeded".into()));
        }
    };
    // Search finished; disarm the disconnect guard so late drops don't poison anything.
    disconnect_guard.1 = true;

    let RouteTaskOut { mut res, virtual_entry, actions, geometry, retried, attempts_pops, seed_dropped, search_ms, payload_ms } = out;

    // Populate the cache on fresh, stable outcomes (Found / genuine NotFound only —
    // budget or cancellation truncations, including truncated-found results whose cost
    // is unproven, are transient and must not stick).
    if cached.is_none() {
        if matches!(res.status, navpath_core::SearchStatus::Found | navpath_core::SearchStatus::NotFound) {
            if let Some(c) = cur.route_cache.as_ref() {
                if let Ok(mut c) = c.lock() {
                    c.put(cache_key, Arc::new((res.clone(), virtual_entry, seed_dropped)));
                    metrics.cache_puts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }

    {
        use std::sync::atomic::Ordering::Relaxed;
        let cache_hit = cached.is_some();
        if cache_hit {
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
            metrics.record_search_ms(search_ms);
        }
        match res.status {
            navpath_core::SearchStatus::Found => metrics.found.fetch_add(1, Relaxed),
            navpath_core::SearchStatus::NotFound => metrics.not_found.fetch_add(1, Relaxed),
            navpath_core::SearchStatus::BudgetExceeded => metrics.budget_exceeded.fetch_add(1, Relaxed),
            navpath_core::SearchStatus::Cancelled => metrics.cancelled.fetch_add(1, Relaxed),
        };
    }

    let duration_ms = start.elapsed().as_millis();
    let length_tiles = res.path.len();
    // only_actions means exactly that: skip the duplicate node-id path in the payload.
    let path = if only_actions { Vec::new() } else { std::mem::take(&mut res.path) };

    let reason = match res.status {
        navpath_core::SearchStatus::BudgetExceeded => Some("budget_exceeded".to_string()),
        navpath_core::SearchStatus::Cancelled => Some("cancelled".to_string()),
        _ => None,
    };
    let degraded = if seed_dropped { Some("seed_dropped".to_string()) } else { None };
    let resp = RouteResponse {
        found: res.found,
        cost: res.cost,
        path,
        length_tiles,
        duration_ms,
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
        search_ms = search_ms,
        payload_ms = payload_ms,
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
        "route request completed"
    );
    Ok(Json(resp))
}

#[derive(Debug, Serialize)]
pub struct ReloadResponse { pub reloaded: bool, pub snapshot_hash: Option<String>, pub loaded_at: u64 }

pub async fn reload(State(state): State<AppState>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let cur = state.current.load();
    let path = cur.path.clone();

    match navpath_core::Snapshot::open(&path) {
        Ok(new_snap) => {
            let new_hash = crate::read_tail_hash_hex(&path);
            // Pre-compute neighbors and globals
            let (neighbors, neighbors_rev, globals, macro_lookup) = crate::engine_adapter::build_neighbor_provider(&new_snap);
            // Pre-compute fairy rings
            let (fairy_rings, node_to_fairy_ring) = crate::engine_adapter::build_fairy_rings(&new_snap);
            let comp_graph = crate::engine_adapter::build_component_graph(&new_snap, &globals, &fairy_rings);
            let canonical_grid = crate::engine_adapter::build_canonical_grid(&new_snap);
            let new_state = SnapshotState {
                path: path.clone(),
                snapshot: Some(Arc::new(new_snap)),
                neighbors: Some(Arc::new(neighbors)),
                neighbors_rev: Some(Arc::new(neighbors_rev)),
                globals: Arc::new(globals),
                macro_lookup: Arc::new(macro_lookup),
                loaded_at_unix: crate::now_unix(),
                snapshot_hash_hex: new_hash.clone(),
                route_cache: crate::new_route_cache(),
                fairy_rings: Arc::new(fairy_rings),
                node_to_fairy_ring: Arc::new(node_to_fairy_ring),
                comp_graph: Some(Arc::new(comp_graph)),
                canonical_grid,
                profile_cache: crate::new_profile_cache(),
            };
            state.current.store(Arc::new(new_state));
            info!(path=?path, hash=?new_hash, "reloaded snapshot");
            let latest = state.current.load();
            Ok(Json(serde_json::json!({
                "reloaded": true,
                "snapshot_hash": latest.snapshot_hash_hex,
                "loaded_at": latest.loaded_at_unix
            })))
        }
        Err(e) => {
            warn!(error=?e, path=?path, "reload failed; keeping old snapshot");
            Err((StatusCode::CONFLICT, e.to_string()))
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

    /// Dive east then surge east: the dive establishes facing, so the 3-walk rule is waived.
    #[test]
    fn same_direction_dive_waives_walk_requirement() {
        let path: Vec<Action> = (1..=20).map(|x| mv(x, 0)).collect();
        let (s, d) = cfgs();
        let out = optimize_with_surge_dive(path, &s, &d);
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
        let out = optimize_with_surge_dive(path, &s, &d);
        let abs = abilities(&out);
        println!("turning: {:?}", abs);
        assert_eq!(abs[0].0, "dive");
        if let Some(surge) = abs.iter().find(|a| a.0 == "surge") {
            assert_ne!(surge.1, abs[0].2, "surge must not fire straight off a turning dive");
        }
    }
}
