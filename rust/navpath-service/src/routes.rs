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
    /// The node-id path, pre-serialized with the payload (T3.13). Absent when empty or
    /// with `only_actions` — exactly where the old `Vec<u32>` field was skipped.
    #[serde(skip_serializing_if = "Option::is_none")] pub path: Option<Box<serde_json::value::RawValue>>,
    pub length_tiles: usize,
    pub duration_ms: u128,
    /// Same clock as `duration_ms`, in microseconds (sub-ms routes read as 0 there).
    pub duration_us: u64,
    /// Present when the search gave up rather than proving its answer
    /// ("budget_exceeded" or "cancelled"). With found=false the goal may still be
    /// reachable; with found=true the returned path is valid but was not proven
    /// optimal (the search was truncated mid-proof). Absent on proven outcomes.
    #[serde(skip_serializing_if = "Option::is_none")] pub reason: Option<&'static str>,
    /// Present when a request-level guarantee was traded for an answer: "seed_dropped"
    /// (the request sent a seed, both seeded attempts exhausted their budgets, and the
    /// served route is the deterministic unseeded optimum) or "seed_ignored" (the
    /// server runs with `--no-seed`).
    #[serde(skip_serializing_if = "Option::is_none")] pub degraded: Option<&'static str>,
    /// Pre-serialized off the reactor (`RawValue` embeds verbatim), so the multi-KB
    /// action list / geometry never serialize on the reactor thread. Bytes are identical
    /// to serializing the typed values here — same serializer, same values.
    #[serde(skip_serializing_if = "Option::is_none")] pub actions: Option<Box<serde_json::value::RawValue>>,
    #[serde(skip_serializing_if = "Option::is_none")] pub geometry: Option<Box<serde_json::value::RawValue>>,
}

/// Serialize a response body ONCE into a buffer sized up front (T3.13), with the
/// headers and error handling of axum's `Json` responder — the bytes on the wire are
/// identical, but the pre-serialized multi-KB parts are copied a single time instead of
/// into a 128-byte buffer that grows by doubling.
fn json_response<T: Serialize>(value: &T, size_hint: usize) -> axum::response::Response {
    use axum::http::{header, HeaderValue};
    use axum::response::IntoResponse;
    let mut buf = Vec::with_capacity(size_hint);
    match serde_json::to_writer(&mut buf, value) {
        Ok(()) => ([(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))], buf).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"))],
            err.to_string(),
        )
            .into_response(),
    }
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

/// The eligible global teleport landing on `dst` with the lowest effective cost
/// (quick-tele lodestones at 2400 ms), with its load-time-parsed metadata. Ties keep the
/// first teleport in snapshot order — the strict `<` replacement rule of the per-payload
/// maps this lookup replaces (T3.8). Called only for the rare path steps that are neither
/// walk nor macro edges, and for a virtual start's entry, so a scan over the ~124
/// globals beats building two hash maps on every payload.
fn eligible_global_to<'g>(
    globals: &'g [engine_adapter::GlobalTeleport],
    mask: &navpath_core::eligibility::EligibilityMask,
    quick_tele: bool,
    dst: u32,
) -> Option<(f32, &'g Arc<serde_json::Value>)> {
    let mut best: Option<(f32, &'g Arc<serde_json::Value>)> = None;
    for g in globals {
        if g.dst != dst || g.reqs.iter().any(|&idx| !mask.is_satisfied(idx)) {
            continue;
        }
        let cost = if quick_tele && g.kind_first == 2 { 2400.0 } else { g.cost };
        if best.is_none_or(|(c, _)| cost < c) {
            best = Some((cost, &g.meta));
        }
    }
    best
}

/// Build the optional actions/geometry payload for a route. Runs in the request's
/// blocking task (or inline for small cache hits) so thousands of per-step
/// constructions never stall the async reactor threads. Emits typed [`Action`]s
/// serialized directly by serde (roadmap 5.3) — no per-tile/per-action
/// `serde_json::Value` assembly. `path` may be a slice of a cached path.
fn build_route_payload(
    job: &RouteJob,
    found: bool,
    path: &[u32],
    virtual_entry: Option<u32>,
) -> (Option<Vec<Action>>, Option<Vec<[i32; 3]>>) {
    if !found {
        return (None, None);
    }
    let snap: &navpath_core::Snapshot = job.snap();
    let globals: &[engine_adapter::GlobalTeleport] = &job.cur.globals;
    let macro_lookup: &engine_adapter::MacroLookup = &job.cur.macro_lookup;
    let fairy_rings: &[engine_adapter::FairyRing] = &job.cur.fairy_rings;
    let node_to_fairy_ring = &job.cur.node_to_fairy_ring;
    let mask = &job.mask;
    let quick_tele = job.quick_tele;

    let coord = |id: u32| -> (i32, i32, i32) { snap.node_coord(id) };

    let mut geometry: Option<Vec<[i32; 3]>> = None;
    if job.return_geometry {
        let mut geom: Vec<[i32; 3]> = Vec::with_capacity(path.len());
        for &id in path {
            let (x, y, p) = coord(id);
            geom.push([x, y, p]);
        }
        geometry = Some(geom);
    }

    if !(job.only_actions || job.return_geometry) {
        return (None, geometry);
    }

    // If we used a virtual start (non-existent start coordinate), the synthetic teleport
    // action goes first; it is completed after the loop from the winning entry teleport.
    let mut virtual_start_action: Option<VirtualStartAction> = None;
    if let Some((vsx, vsy, vsp)) = job.virtual_start_from {
        let entry_id = virtual_entry.unwrap_or(job.sid);
        let (actual_x, actual_y, actual_p) = coord(entry_id);
        virtual_start_action = Some(VirtualStartAction {
            kind: "global_teleport".to_string(),
            from: MinMax::point(vsx, vsy, vsp),
            to: MinMax::point(actual_x, actual_y, actual_p),
            cost_ms: serde_json::Number::from(0),
            metadata: serde_json::json!({"reason": "start_coordinate_not_found"}),
        });
    }

    let mut acts: Vec<Action> =
        Vec::with_capacity(path.len().saturating_sub(1) + usize::from(virtual_start_action.is_some()));
    if virtual_start_action.is_some() {
        // Placeholder for slot 0, overwritten below (no `insert(0, ..)` shift).
        acts.push(Action::Teleport(TeleportAction {
            kind: "teleport",
            from: MinMax::point(0, 0, 0),
            to: MinMax::point(0, 0, 0),
            cost_ms: 0,
        }));
    }

    for w in path.windows(2) {
        let (u, v) = (w[0], w[1]);
        let (x1, y1, p1) = coord(u);
        let (x2, y2, p2) = coord(v);

        if let Some(idxs) = macro_lookup.get(&(u, v)) {
            // Cheapest parallel edge the profile may use; requirement lists were decoded
            // at load (T3.7), so no candidate's metadata is parsed here.
            let mut chosen: Option<(usize, f32)> = None;
            for &idx_u32 in idxs {
                let idx = idx_u32 as usize;
                if !macro_lookup.allowed(idx, mask) {
                    continue;
                }
                let mut cost_ms = snap.macro_w().get(idx).copied().unwrap_or(0.0);
                let k = snap.macro_kind_first().get(idx).copied().unwrap_or(0);
                if quick_tele && k == 2 {
                    cost_ms = 2400.0;
                }
                if chosen.is_none_or(|(_, best_cost)| cost_ms < best_cost) {
                    chosen = Some((idx, cost_ms));
                }
            }
            let (idx, mut cost_ms) = chosen.unwrap_or_else(|| {
                let idx = idxs.first().copied().unwrap_or(0) as usize;
                (idx, snap.macro_w().get(idx).copied().unwrap_or(0.0))
            });
            let mut meta = macro_lookup.meta_value(snap, idx);

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
            } else if let Some((gc, meta)) = eligible_global_to(globals, mask, quick_tele, v) {
                // Metadata stays behind the shared Arc — serialization reads through it.
                let meta = meta.clone();
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

    // If we had a virtual start, fill slot 0 with the synthetic first action, derived from
    // the selected global teleport's metadata/cost.
    if let Some(mut virtual_action) = virtual_start_action {
        if let Some(entry_id) = virtual_entry {
            if let Some((gc, meta)) = eligible_global_to(globals, mask, quick_tele, entry_id) {
                virtual_action.cost_ms = serde_json::Number::from_f64(gc as f64)
                    .unwrap_or_else(|| serde_json::Number::from(0));
                virtual_action.kind = global_step_kind(meta).to_string();
                if let Some(obj) = virtual_action.metadata.as_object_mut() {
                    // The metadata subtree is mutated here, so this one clones out of
                    // the Arc (exactly as before).
                    obj.insert("teleport".to_string(), (**meta).clone());
                }
            }
        }
        acts[0] = Action::VirtualStart(Box::new(virtual_action));
    }

    // The tile the character stands on before the first action. For a virtual start the
    // synthetic teleport action already occupies index 0 and carries it, but on the normal
    // path nothing in `acts` records it — see `optimize_with_surge_dive`.
    let route_origin = path.first().map(|&id| coord(id));

    // Apply surge/dive optimization to the actions
    (Some(optimize_with_surge_dive(acts, &job.surge, &job.dive, route_origin)), geometry)
}

/// Everything one route's search and payload work needs, shared by its blocking tasks
/// behind ONE `Arc` (T3.15: the handler used to clone ~15 separate Arcs and the
/// eligibility mask twice per request).
struct RouteJob {
    /// The snapshot generation this request resolved against (its snapshot is loaded).
    cur: Arc<SnapshotState>,
    /// Per-profile search artifacts; None on cache/sub-path hits (no search runs).
    artifacts: Option<Arc<engine_adapter::ProfileArtifacts>>,
    mask: navpath_core::eligibility::EligibilityMask,
    sid: u32,
    gid: u32,
    used_virtual_start: bool,
    seed: Option<u64>,
    quick_tele: bool,
    return_geometry: bool,
    only_actions: bool,
    surge: SurgeConfig,
    dive: DiveConfig,
    virtual_start_from: Option<(i32, i32, i32)>,
    /// Fresh proven results are indexed for the sub-path cache (unseeded, on-graph start,
    /// cache enabled) — in the blocking task, off the reactor (T3.6).
    index_subpath: bool,
}

/// The pre-serialized halves of a response (T3.13).
struct Payload {
    path: Option<Box<serde_json::value::RawValue>>,
    actions: Option<Box<serde_json::value::RawValue>>,
    geometry: Option<Box<serde_json::value::RawValue>>,
    payload_us: u64,
}

impl Payload {
    fn len(&self) -> usize {
        [&self.path, &self.actions, &self.geometry]
            .iter()
            .map(|p| p.as_ref().map_or(0, |r| r.get().len()))
            .sum()
    }
}

impl RouteJob {
    fn snap(&self) -> &Arc<navpath_core::Snapshot> {
        self.cur.snapshot.as_ref().expect("route jobs are only built for a loaded snapshot")
    }

    /// Build and serialize the payload for `path` (a full result or a cached slice).
    fn payload(&self, found: bool, path: &[u32], virtual_entry: Option<u32>) -> Payload {
        let t_payload = std::time::Instant::now();
        let (actions, geometry) = build_route_payload(self, found, path, virtual_entry);
        let actions = actions.map(|a| serde_json::value::to_raw_value(&a).expect("actions serialize"));
        let geometry = geometry.map(|g| serde_json::value::to_raw_value(&g).expect("geometry serialize"));
        // only_actions means exactly that: skip the duplicate node-id path in the payload.
        let path = if self.only_actions || path.is_empty() {
            None
        } else {
            Some(serde_json::value::to_raw_value(path).expect("path serialize"))
        };
        Payload { path, actions, geometry, payload_us: t_payload.elapsed().as_micros() as u64 }
    }

    /// One fresh search on `engine`, observing `cancel`. The contexts return to the pool
    /// when this returns, before payload building.
    fn run_search(
        &self,
        engine: engine_adapter::EngineChoice,
        cancel: &std::sync::atomic::AtomicBool,
        pool: &Arc<crate::ContextPool>,
    ) -> (engine_adapter::SearchOutcome, Option<u32>) {
        let arts = self.artifacts.as_ref().expect("profile artifacts resolved for fresh searches");
        let cur = &self.cur;
        let neighbors = cur.neighbors.clone().expect("neighbors are loaded with the snapshot");
        let mut lease = pool.checkout();
        if self.used_virtual_start {
            engine_adapter::run_route_with_requirements_virtual_start(
                self.snap().clone(),
                neighbors,
                cur.neighbors_rev.clone(),
                self.gid,
                self.seed,
                Some(cancel),
                arts,
                cur.canonical_grid.clone(),
                engine,
                &mut lease,
            )
        } else {
            (
                engine_adapter::run_route_with_requirements_and_fairy_rings(
                    self.snap().clone(),
                    neighbors,
                    cur.neighbors_rev.clone(),
                    self.sid,
                    self.gid,
                    &self.mask,
                    self.seed,
                    Some(cancel),
                    arts,
                    cur.canonical_grid.clone(),
                    engine,
                    &mut lease,
                ),
                None,
            )
        }
    }

    /// Wrap a search outcome for the handler: share the result (one allocation for the
    /// response, the route cache and the sub-path cache), index it for sub-path reuse,
    /// and build + serialize the payload — all on the calling blocking thread.
    fn finish(&self, outcome: engine_adapter::SearchOutcome, virtual_entry: Option<u32>, search_us: u64) -> RouteTaskOut {
        let res = Arc::new(outcome.res);
        let payload = self.payload(res.found, &res.path, virtual_entry);
        let subpath_rec = if self.index_subpath { crate::PathRecord::new(res.clone()) } else { None };
        RouteTaskOut {
            res,
            virtual_entry,
            payload,
            subpath_rec,
            retried: outcome.retried,
            attempts_pops: outcome.attempts_pops,
            seed_dropped: outcome.seed_dropped,
            engine: outcome.engine,
            search_us,
        }
    }
}

/// Everything the blocking task computes for one fresh search; carried back to the
/// handler for the caches, the response, metrics, and the log line.
struct RouteTaskOut {
    res: Arc<navpath_core::SearchResult>,
    virtual_entry: Option<u32>,
    payload: Payload,
    subpath_rec: Option<Arc<crate::PathRecord>>,
    retried: bool,
    attempts_pops: [u32; 3],
    seed_dropped: bool,
    engine: &'static str,
    search_us: u64,
}

/// A request answered without a search: a route-cache entry or a slice of a cached
/// optimal path.
enum Hit {
    Cached(crate::RouteCacheEntry),
    Subpath(crate::SubpathHit),
}

impl Hit {
    fn path(&self) -> &[u32] {
        match self {
            Hit::Cached(e) => &e.res.path,
            Hit::Subpath(h) => h.path(),
        }
    }
}

/// What the response, the metrics and the log line need from a finished request.
struct Served {
    found: bool,
    status: navpath_core::SearchStatus,
    cost: f32,
    length_tiles: usize,
    pops: u32,
    pops_f: u32,
    pops_b: u32,
    retried: bool,
    attempts_pops: [u32; 3],
    seed_dropped: bool,
    engine: &'static str,
    search_us: u64,
    payload: Payload,
}

impl Served {
    fn from_hit(hit: &Hit, payload: Payload) -> Self {
        match hit {
            Hit::Cached(e) => Served {
                found: e.res.found,
                status: e.res.status,
                cost: e.res.cost,
                length_tiles: e.res.path.len(),
                pops: e.res.pops,
                pops_f: e.res.pops_f,
                pops_b: e.res.pops_b,
                retried: false,
                attempts_pops: [0, 0, 0],
                seed_dropped: e.seed_dropped,
                engine: "cache",
                search_us: 0,
                payload,
            },
            Hit::Subpath(h) => Served {
                found: true,
                status: navpath_core::SearchStatus::Found,
                cost: h.cost(),
                length_tiles: h.pg - h.ps + 1,
                pops: 0,
                pops_f: 0,
                pops_b: 0,
                retried: false,
                attempts_pops: [0, 0, 0],
                seed_dropped: false,
                engine: "cache",
                search_us: 0,
                payload,
            },
        }
    }

    fn from_search(out: RouteTaskOut) -> Self {
        Served {
            found: out.res.found,
            status: out.res.status,
            cost: out.res.cost,
            length_tiles: out.res.path.len(),
            pops: out.res.pops,
            pops_f: out.res.pops_f,
            pops_b: out.res.pops_b,
            retried: out.retried,
            attempts_pops: out.attempts_pops,
            seed_dropped: out.seed_dropped,
            engine: out.engine,
            search_us: out.search_us,
            payload: out.payload,
        }
    }
}

/// Cache hits whose payload is at most this many path nodes (or that request no
/// actions/geometry at all) are answered on the reactor (T3.11): the payload is
/// microseconds, less than the `spawn_blocking` round trip it used to take.
const INLINE_HIT_PAYLOAD_NODES: usize = 256;

/// Process-lifetime service counters (see [`crate::Metrics`]) plus the live route-cache
/// state and the policy in force — so a zero hit rate can be diagnosed from one call:
/// `cache_miss_seed` is how many requests `NAVPATH_CACHE_IGNORE_SEED=1` would convert
/// into hits, `cache_miss_cold` is how many no cache policy can help.
pub async fn stats(State(state): State<AppState>) -> Json<serde_json::Value> {
    let cur = state.current.load();
    let mut out = state.metrics.snapshot_json();
    let (entries, capacity) = cur
        .route_cache
        .as_ref()
        .and_then(|c| c.lock().ok().map(|c| (c.len(), c.cap().get())))
        .unwrap_or((0, 0));
    if let Some(obj) = out.as_object_mut() {
        obj.insert(
            "route_cache".to_string(),
            serde_json::json!({
                "enabled": cur.route_cache.is_some(),
                "entries": entries,
                "capacity": capacity,
                "ignore_seed": crate::cache_ignore_seed(),
            }),
        );
        obj.insert("subpath_cache".to_string(), serde_json::json!({
            "enabled": cur.subpath_cache.is_some(),
            "paths_per_profile": crate::subpath_cache_paths(),
        }));
        obj.insert("seeding_disabled".to_string(), serde_json::json!(seeding_disabled()));
        obj.insert("race_enabled".to_string(), serde_json::json!(race_enabled()));
        obj.insert("race".to_string(), serde_json::json!({
            "primary": race_primary().as_str(),
            "hedge_ms": race_hedge_delay().as_secs_f64() * 1000.0,
            "gate": race_gate(),
        }));
        obj.insert("search_permits".to_string(), serde_json::json!({
            "total": state.search_permits.total(),
            "available": state.search_permits.available(),
        }));
        obj.insert("ctx_pool".to_string(), serde_json::json!({
            "idle": state.ctx_pool.idle(),
            "fresh_allocations": state.ctx_pool.fresh_allocations(),
        }));
        obj.insert("ready".to_string(), serde_json::json!(state.ready.load(std::sync::atomic::Ordering::Acquire)));
    }
    Json(out)
}

/// `NAVPATH_RACE=1`: hedged engine race (uni and bidir, first stable result wins, loser
/// cancelled). Default off. The second engine (the hedge) needs a spare search permit
/// (see [`crate::SearchPermits::try_acquire_hedge`]); without one the request runs the
/// primary engine alone. See [`race_hedge_delay`], [`race_gate`] and
/// [`race_primary`] for when the hedge starts.
pub fn race_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        matches!(std::env::var("NAVPATH_RACE").ok().as_deref().map(str::trim), Some("1") | Some("true"))
    })
}

/// `NAVPATH_RACE_HEDGE_MS` (T3.2a, default 0): start the hedge engine only if the
/// primary has not finished within this many milliseconds; `0` starts both engines
/// together. Applies to the misses the gate lets race. Tokio's timer has 1 ms
/// granularity, so a delay of `D` starts the hedge between `D` and `D+1` ms in.
/// Default 0 because a delay only pays where the gate is off: over the same sweeps a
/// 1 ms delay saved 6-11% CPU for +0-11% latency sum (long searches dominate CPU and
/// still hedge), while the gate removes most hedges with no latency cost.
pub fn race_hedge_delay() -> std::time::Duration {
    static D: OnceLock<std::time::Duration> = OnceLock::new();
    *D.get_or_init(|| {
        let ms = std::env::var("NAVPATH_RACE_HEDGE_MS").ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
            .unwrap_or(RACE_HEDGE_MS_DEFAULT);
        std::time::Duration::from_secs_f64(ms / 1000.0)
    })
}
const RACE_HEDGE_MS_DEFAULT: f64 = 0.0;

/// `NAVPATH_RACE_GATE` (T3.2b, default on; `0` races every miss): hedge only routes the
/// [`engine_adapter::RaceHint`] predicts the second engine can win — for a bidir
/// primary, teleport-dominated routes or `h(start)` >= 20 s; for a JPS/uni primary,
/// heuristic-blind routes (`h = 0`). Every other miss runs the primary engine alone.
/// Measured (examples/race_sweep, see `RaceHint`): latency sum within 0-2% of racing
/// every miss with identical p99/max, 4-49% less race CPU, and with a JPS primary the
/// hedge (second permit, second context lease, second blocking thread) runs on 2-3% of
/// misses instead of all of them.
pub fn race_gate() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| !matches!(std::env::var("NAVPATH_RACE_GATE").ok().as_deref().map(str::trim), Some("0") | Some("false")))
}

/// Which engine a race starts first; the other one is the hedge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RacePrimary {
    /// The engine expected to win: the unidirectional engine when jump-point expansion
    /// applies to the search (`NAVPATH_JPS=1`, canonical grid loaded, unseeded), else
    /// bidirectional. Measured with `examples/race_sweep` (2026-09-25, 300-400 LCG pairs
    /// per config): JPS beats bidir on 383-386/400 all-eligible pairs and 294/300 gated
    /// ones; without JPS (seeded, or `NAVPATH_JPS=0`) bidir is the better single engine
    /// (sum of per-pair times ~0.5-0.6x of plain uni).
    Auto,
    Uni,
    Bidir,
}

impl RacePrimary {
    pub fn as_str(self) -> &'static str {
        match self {
            RacePrimary::Auto => "auto",
            RacePrimary::Uni => "uni",
            RacePrimary::Bidir => "bidir",
        }
    }
}

/// `NAVPATH_RACE_PRIMARY=auto|uni|bidir` (default `auto`, see [`RacePrimary`]). The
/// primary is also the engine a race-eligible miss runs alone when the hedge does not
/// start (gated, finished inside the delay, or no spare permit).
pub fn race_primary() -> RacePrimary {
    static P: OnceLock<RacePrimary> = OnceLock::new();
    *P.get_or_init(|| match std::env::var("NAVPATH_RACE_PRIMARY").ok().as_deref().map(str::trim) {
        Some("uni") | Some("jps") => RacePrimary::Uni,
        Some("bidir") => RacePrimary::Bidir,
        _ => RacePrimary::Auto,
    })
}

/// Resolve the race primary for one request.
fn primary_is_uni(seed: Option<u64>, canonical_loaded: bool) -> bool {
    match race_primary() {
        RacePrimary::Uni => true,
        RacePrimary::Bidir => false,
        RacePrimary::Auto => seed.is_none() && canonical_loaded && navpath_core::engine::search::jps_enabled(),
    }
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

type RouteResult = Result<axum::response::Response, (StatusCode, String)>;

pub async fn route(State(state): State<AppState>, Json(mut req): Json<RouteRequest>) -> RouteResult {
    let start = std::time::Instant::now();
    // Seed kill switch: clear before ANY reader (cache key, shadow attribution,
    // search) so the request is unseeded everywhere, not just in the engine.
    let seed_ignored = seeding_disabled() && req.seed.take().is_some();
    let metrics = state.metrics.clone();
    metrics.requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if !state.ready.load(std::sync::atomic::Ordering::Acquire) {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "warming up (snapshot populate / context pre-warm); retry".into()));
    }
    // One reference to this request's snapshot generation; everything below borrows it.
    let cur: Arc<SnapshotState> = state.current.load_full();
    let Some(snap) = cur.snapshot.as_ref() else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "snapshot not loaded".into()));
    };
    let Some(neighbors) = cur.neighbors.as_ref() else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "neighbors not loaded".into()));
    };
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
    let mask_bits = crate::pack_mask_bits(&mask.satisfied);
    let profile_key: crate::ProfileKey = (mask_bits.clone(), quick_tele);
    let cache_key = crate::RouteCacheKey {
        virtual_start: used_virtual_start,
        sid: if used_virtual_start { 0 } else { sid },
        gid,
        mask_bits,
        quick_tele,
        seed: if crate::cache_ignore_seed() { None } else { req.seed },
    };
    let cached: Option<crate::RouteCacheEntry> = cur
        .route_cache
        .as_ref()
        .and_then(|c| c.lock().ok().and_then(|mut c| c.get(&cache_key).cloned()));

    // Exact sub-path reuse (crate::SubpathCache): both endpoints on a cached optimal
    // path for this profile => serve the slice, no search. Virtual starts never qualify.
    let mut goal_known = false;
    // Cached slices carry UNSEEDED (base-cost) `path_g`; a seeded request may use them
    // only under the seed-blind cache policy, exactly like the route cache.
    let subpath_hit: Option<crate::SubpathHit> = if cached.is_none() && !used_virtual_start && (req.seed.is_none() || crate::cache_ignore_seed()) {
        cur.subpath_cache.as_ref().and_then(|c| {
            let (hit, known) = crate::subpath_lookup(c, &profile_key, sid, gid);
            goal_known = known;
            hit
        })
    } else {
        None
    };
    let hit: Option<Hit> = match (cached, subpath_hit) {
        (Some(e), _) => Some(Hit::Cached(e)),
        (None, Some(h)) => Some(Hit::Subpath(h)),
        (None, None) => None,
    };
    let subpath_served = matches!(hit, Some(Hit::Subpath(_)));

    // Attribute the miss (see crate::SeedShadow). A seeded request whose seed-blind key
    // is already known missed *because of the seed*; anything else is a genuinely new
    // (endpoints, profile) pair. The shadow only exists under per-seed keying (T3.12),
    // and the seed-blind key is rebuilt only where it is used.
    let seed_blind_key = || crate::RouteCacheKey { seed: None, ..cache_key.clone() };
    let cache_outcome = if subpath_served {
        crate::CacheOutcome::Subpath
    } else if cur.route_cache.is_none() {
        crate::CacheOutcome::Disabled
    } else if hit.is_some() {
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
        if hit.is_none() && goal_known {
            metrics.cache_miss_goal_known.fetch_add(1, Relaxed);
        }
    }

    let virtual_start_from = if used_virtual_start {
        req.start.as_ref().map(|c| (c.wx, c.wy, c.plane))
    } else {
        None
    };
    let seed = req.seed;
    let mut job = RouteJob {
        cur: cur.clone(),
        artifacts: None,
        mask,
        sid,
        gid,
        used_virtual_start,
        seed,
        quick_tele,
        return_geometry: req.options.return_geometry,
        only_actions: req.options.only_actions,
        surge: req.surge,
        dive: req.dive,
        virtual_start_from,
        index_subpath: cur.subpath_cache.is_some() && !used_virtual_start && seed.is_none(),
    };

    // ---- Cache / sub-path hit: no search, no permit. ----
    if let Some(hit) = hit {
        let job = Arc::new(job);
        let virtual_entry = match &hit {
            Hit::Cached(e) => e.virtual_entry,
            Hit::Subpath(_) => None,
        };
        let no_payload = !(job.only_actions || job.return_geometry);
        let served = if no_payload || hit.path().len() <= INLINE_HIT_PAYLOAD_NODES {
            let payload = job.payload(hit_found(&hit), hit.path(), virtual_entry);
            Served::from_hit(&hit, payload)
        } else {
            let task_job = job.clone();
            tokio::task::spawn_blocking(move || {
                let payload = task_job.payload(hit_found(&hit), hit.path(), virtual_entry);
                Served::from_hit(&hit, payload)
            })
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        };
        return Ok(respond(&metrics, start, served, true, subpath_served, cache_outcome, seed_ignored));
    }

    // Per-profile search artifacts (roadmap 5.4): MacroFilters, eligible globals, fairy
    // sets and the reachability view are pure functions of (snapshot, exact mask bits,
    // quick_tele), so misses resolve them from the per-snapshot LRU instead of
    // rebuilding. Cheap (a lock + at worst one ~1.7k-slot scan). Resolved before the
    // precheck, which reads the profile's reachability view (T3.14).
    let artifacts: Arc<engine_adapter::ProfileArtifacts> = {
        let hit = cur.profile_cache.lock().ok().and_then(|mut c| c.get(&profile_key).cloned());
        match hit {
            Some(a) => a,
            None => {
                let built = Arc::new(engine_adapter::build_profile_artifacts(
                    neighbors.as_ref(),
                    cur.neighbors_rev.as_deref(),
                    cur.globals.as_slice(),
                    cur.fairy_rings.as_slice(),
                    &job.mask,
                    quick_tele,
                ));
                if let Ok(mut c) = cur.profile_cache.lock() {
                    c.put(profile_key.clone(), built.clone());
                }
                built
            }
        }
    };

    // Exact reachability precheck (roadmap 4.1): eligibility never gates walk edges,
    // so "can this goal be reached at all under this profile" is decided on the
    // ~491-component condensation in microseconds — BEFORE a permit, a blocking
    // thread, or a search context is committed. Every rejection here is a budget-capped
    // ~1.5M-pop flood (plus its retry) that never ran. The verdict is exact, so the
    // response is identical to what the flood would have produced.
    if let Some(cg) = cur.comp_graph.as_ref() {
        let comps = snap.comp_ids();
        let start_comp = if used_virtual_start { None } else { Some(comps[sid as usize]) };
        let goal_comp = comps[gid as usize];
        if !artifacts.reach(cg, &job.mask).reachable(start_comp, goal_comp) {
            use std::sync::atomic::Ordering::Relaxed;
            metrics.precheck_rejects.fetch_add(1, Relaxed);
            metrics.not_found.fetch_add(1, Relaxed);
            let duration_us = start.elapsed().as_micros() as u64;
            let duration_ms = (duration_us / 1000) as u128;
            info!(duration_ms, sid, gid, virtual_start = used_virtual_start,
                  "route rejected by component reachability precheck");
            let resp = RouteResponse {
                found: false,
                cost: f32::INFINITY,
                path: None,
                length_tiles: 0,
                duration_ms,
                duration_us,
                reason: None,
                degraded: None,
                actions: None,
                geometry: None,
            };
            return Ok(json_response(&resp, 128));
        }
    }

    // Offload search to a blocking thread, bounded by the search semaphore so a burst of
    // slow queries cannot pin hundreds of blocking-pool threads (each holding
    // node-sized search contexts). Overload fails fast instead of queueing floods. The
    // permit moves into the blocking task and is released the moment the search itself
    // finishes — payload building and response serialization must not count against
    // search admission.
    let Some(permit) = state.search_permits.try_acquire() else {
        metrics.semaphore_rejects.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        warn!(sid, gid, "search capacity exhausted; rejecting with 503");
        return Err((StatusCode::SERVICE_UNAVAILABLE, "search capacity exhausted; retry".into()));
    };

    // Hedged race (`NAVPATH_RACE=1`): the uni and bidir engines are both exact, so the
    // served cost is identical either way; the race buys the per-pair minimum of two
    // engines whose relative speed swings 3-5x in both directions depending on whether
    // the route is walk- or teleport-dominated (docs/route_latency_improvements_2026-09-17.md
    // §1.2/§2.1). The primary engine starts at once; the hedge (the other engine) starts
    // only if the primary is still running after `NAVPATH_RACE_HEDGE_MS`, only when the
    // predictive gate (if on) judges the route worth it, and only with a spare permit.
    let race_possible = race_enabled() && engine_adapter::bidir_enabled() && cur.neighbors_rev.is_some();
    let uni_primary = primary_is_uni(seed, cur.canonical_grid.is_some());
    let (primary, hedge) = if uni_primary {
        (engine_adapter::EngineChoice::Uni, engine_adapter::EngineChoice::Bidir)
    } else {
        (engine_adapter::EngineChoice::Bidir, engine_adapter::EngineChoice::Uni)
    };
    let race = race_possible && {
        let worth = !race_gate() || {
            let start_node = if used_virtual_start { None } else { Some(sid) };
            engine_adapter::race_hint(snap, &artifacts, start_node, gid).worth_racing(uni_primary)
        };
        if !worth {
            metrics.race_gated.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        worth
    };
    // Without a race the primary still runs when racing is configured (so gated routes
    // use the engine the race would have started first); otherwise the shipped policy.
    let single_engine = if race_possible { primary } else { engine_adapter::EngineChoice::Policy };
    job.artifacts = Some(artifacts);
    let job = Arc::new(job);

    // Cooperative cancellation: one flag per engine (the race cancels only the loser);
    // the disconnect guard and the route deadline flip every flag. The engine checks
    // its flag every 1024 pops, and the retry ladder never starts a rung once it is set.
    let cancel_flags: Arc<[Arc<std::sync::atomic::AtomicBool>]> =
        (0..if race { 2 } else { 1 }).map(|_| Arc::new(std::sync::atomic::AtomicBool::new(false))).collect();
    struct CancelOnDrop(Arc<[Arc<std::sync::atomic::AtomicBool>]>, bool);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            if !self.1 {
                for f in self.0.iter() {
                    f.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }
    let mut disconnect_guard = CancelOnDrop(cancel_flags.clone(), false);
    let ctx_pool = state.ctx_pool.clone();

    let work: std::pin::Pin<Box<dyn std::future::Future<Output = Result<RouteTaskOut, String>> + Send>> = if !race {
        let job = job.clone();
        let cancel = cancel_flags[0].clone();
        let join = tokio::task::spawn_blocking(move || {
            let t_search = std::time::Instant::now();
            let (outcome, virtual_entry) = job.run_search(single_engine, &cancel, &ctx_pool);
            let search_us = t_search.elapsed().as_micros() as u64;
            drop(permit);
            job.finish(outcome, virtual_entry, search_us)
        });
        Box::pin(async move { join.await.map_err(|e| e.to_string()) })
    } else {
        Box::pin(run_race(
            RaceSetup {
                job: job.clone(),
                pool: ctx_pool,
                permits: state.search_permits.clone(),
                metrics: metrics.clone(),
                flags: cancel_flags.clone(),
                primary,
                hedge,
                delay: race_hedge_delay(),
            },
            permit,
        ))
    };
    let out = match tokio::time::timeout(route_deadline(), work).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e)),
        Err(_) => {
            for f in cancel_flags.iter() {
                f.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            metrics.deadline_timeouts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            warn!(sid, gid, deadline_ms = route_deadline().as_millis() as u64, "route deadline exceeded; returning 504");
            return Err((StatusCode::GATEWAY_TIMEOUT, "route deadline exceeded".into()));
        }
    };
    // Search finished; disarm the disconnect guard so late drops don't poison anything.
    disconnect_guard.1 = true;

    // Populate the caches: the sub-path index was built in the blocking task (only for
    // unseeded, on-graph, proven results), and both caches share the result's Arc.
    let mut out = out;
    if let (Some(c), Some(rec)) = (cur.subpath_cache.as_ref(), out.subpath_rec.take()) {
        crate::subpath_insert(c, profile_key, rec);
    }
    // The route cache takes fresh, stable outcomes (Found / genuine NotFound only —
    // budget or cancellation truncations, including truncated-found results whose cost
    // is unproven, are transient and must not stick).
    if matches!(out.res.status, navpath_core::SearchStatus::Found | navpath_core::SearchStatus::NotFound) {
        if let Some(c) = cur.route_cache.as_ref() {
            // Keeping the attribution index in step with what the cache actually holds
            // is what stops `miss_seed` from claiming a hit the policy could not have
            // delivered. Built before the key is moved into the cache.
            let shadow = cur.seed_shadow.as_ref().map(|s| (s, seed_blind_key()));
            if let Ok(mut c) = c.lock() {
                c.put(
                    cache_key,
                    crate::RouteCacheEntry { res: out.res.clone(), virtual_entry: out.virtual_entry, seed_dropped: out.seed_dropped },
                );
                metrics.cache_puts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            if let Some((s, key)) = shadow {
                if let Ok(mut s) = s.lock() {
                    s.put(key, ());
                }
            }
        }
    }

    Ok(respond(&metrics, start, Served::from_search(out), false, false, cache_outcome, seed_ignored))
}

fn hit_found(hit: &Hit) -> bool {
    match hit {
        Hit::Cached(e) => e.res.found,
        Hit::Subpath(_) => true,
    }
}

/// Metrics, optional dump, the per-request log line, and the single-pass response body.
fn respond(
    metrics: &crate::Metrics,
    start: std::time::Instant,
    s: Served,
    cache_hit: bool,
    subpath_served: bool,
    cache_outcome: crate::CacheOutcome,
    seed_ignored: bool,
) -> axum::response::Response {
    {
        use std::sync::atomic::Ordering::Relaxed;
        if subpath_served {
            metrics.cache_subpath_hits.fetch_add(1, Relaxed);
        } else if cache_hit {
            metrics.cache_hits.fetch_add(1, Relaxed);
        } else {
            metrics.searches.fetch_add(1, Relaxed);
            if s.retried {
                metrics.retries.fetch_add(1, Relaxed);
                if s.found {
                    metrics.retry_found.fetch_add(1, Relaxed);
                }
            }
            metrics.record_pops(s.pops as u64);
            metrics.record_search_ms(s.search_us / 1000);
            metrics.record_search_us(s.search_us, s.pops as u64);
        }
        match s.status {
            navpath_core::SearchStatus::Found => metrics.found.fetch_add(1, Relaxed),
            navpath_core::SearchStatus::NotFound => metrics.not_found.fetch_add(1, Relaxed),
            navpath_core::SearchStatus::BudgetExceeded => metrics.budget_exceeded.fetch_add(1, Relaxed),
            navpath_core::SearchStatus::Cancelled => metrics.cancelled.fetch_add(1, Relaxed),
        };
    }

    let duration_us = start.elapsed().as_micros() as u64;
    let duration_ms = (duration_us / 1000) as u128;
    let reason = match s.status {
        navpath_core::SearchStatus::BudgetExceeded => Some("budget_exceeded"),
        navpath_core::SearchStatus::Cancelled => Some("cancelled"),
        _ => None,
    };
    let degraded = if s.seed_dropped {
        Some("seed_dropped")
    } else if seed_ignored {
        // The client sent a seed but the server runs with --no-seed: the served route
        // is the deterministic unseeded optimum.
        Some("seed_ignored")
    } else {
        None
    };
    let payload_us = s.payload.payload_us;
    let size_hint = s.payload.len() + 256;
    let resp = RouteResponse {
        found: s.found,
        cost: s.cost,
        path: s.payload.path,
        length_tiles: s.length_tiles,
        duration_ms,
        duration_us,
        reason,
        degraded,
        actions: s.payload.actions,
        geometry: s.payload.geometry,
    };
    if let Some(dump_path) = result_dump_path() {
        if let Ok(bytes) = serde_json::to_vec_pretty(&resp) {
            let _ = std::fs::write(dump_path, bytes);
        }
    }
    info!(
        duration_ms = duration_ms,
        search_ms = s.search_us / 1000,
        payload_ms = payload_us / 1000,
        duration_us = duration_us,
        search_us = s.search_us,
        payload_us = payload_us,
        // Memory-behaviour signal: ~150-230 warm, tens of thousands on a cold page cache.
        ns_per_pop = if s.pops > 0 { s.search_us * 1000 / s.pops as u64 } else { 0 },
        found = s.found,
        cost = s.cost,
        length = s.length_tiles,
        status = ?s.status,
        pops = s.pops,
        pops_f = s.pops_f,
        pops_b = s.pops_b,
        retried = s.retried,
        first_attempt_pops = s.attempts_pops[0],
        retry_pops = s.attempts_pops[1],
        retry_unseeded_pops = s.attempts_pops[2],
        seed_dropped = s.seed_dropped,
        cache_hit = cache_hit,
        // Which engine served it: uni | jps | bidir | cache (race winners are uni/jps/bidir).
        engine = s.engine,
        // Why the cache did/didn't serve this: hit | subpath | miss_seed | miss_cold | off.
        cache = cache_outcome.as_str(),
        "route request completed"
    );
    json_response(&resp, size_hint)
}

/// Inputs of one hedged race (see [`run_race`]).
struct RaceSetup {
    job: Arc<RouteJob>,
    pool: Arc<crate::ContextPool>,
    permits: Arc<crate::SearchPermits>,
    metrics: Arc<crate::Metrics>,
    /// Cancel flags: [primary, hedge].
    flags: Arc<[Arc<std::sync::atomic::AtomicBool>]>,
    primary: engine_adapter::EngineChoice,
    hedge: engine_adapter::EngineChoice,
    delay: std::time::Duration,
}

enum RaceMsg {
    /// The arm that claimed the request, with its payload built on its own thread.
    Done(RouteTaskOut),
    /// An arm that did not claim: truncated (budget/cancel), or stable but second.
    Truncated(engine_adapter::SearchOutcome, Option<u32>, u64),
}

fn stable(o: &engine_adapter::SearchOutcome) -> bool {
    matches!(o.res.status, navpath_core::SearchStatus::Found | navpath_core::SearchStatus::NotFound)
}

/// Start one race arm on the blocking pool. The first arm to finish with a STABLE result
/// (Found / genuine NotFound) claims the request: it cancels the other engine and builds
/// the payload on its own thread, so the winning path costs no extra task hop. A
/// truncated result (budget/cancel) never claims.
fn spawn_race_arm(
    setup: &RaceSetup,
    engine: engine_adapter::EngineChoice,
    idx: usize,
    permit: tokio::sync::OwnedSemaphorePermit,
    claimed: &Arc<std::sync::atomic::AtomicBool>,
    tx: &tokio::sync::mpsc::Sender<RaceMsg>,
) {
    let (job, pool, flags) = (setup.job.clone(), setup.pool.clone(), setup.flags.clone());
    let (claimed, tx) = (claimed.clone(), tx.clone());
    tokio::task::spawn_blocking(move || {
        let t_search = std::time::Instant::now();
        let (outcome, virtual_entry) = job.run_search(engine, &flags[idx], &pool);
        let search_us = t_search.elapsed().as_micros() as u64;
        drop(permit);
        let won = stable(&outcome)
            && claimed
                .compare_exchange(false, true, std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Acquire)
                .is_ok();
        if won {
            if let Some(other) = flags.get(1 - idx) {
                other.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            let _ = tx.blocking_send(RaceMsg::Done(job.finish(outcome, virtual_entry, search_us)));
        } else {
            // Loser (or truncated): the receiver may already be gone.
            let _ = tx.blocking_send(RaceMsg::Truncated(outcome, virtual_entry, search_us));
        }
    });
}

/// Run the primary engine and, per the hedge policy, the other engine; serve the first
/// stable result. If both arms truncate, the better truncated result is served.
async fn run_race(setup: RaceSetup, primary_permit: tokio::sync::OwnedSemaphorePermit) -> Result<RouteTaskOut, String> {
    use std::sync::atomic::Ordering::Relaxed;
    let claimed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<RaceMsg>(2);
    let (primary, hedge, delay) = (setup.primary, setup.hedge, setup.delay);
    spawn_race_arm(&setup, primary, 0, primary_permit, &claimed, &tx);

    // Start the hedge: needs a spare permit (T3.1). Our sender is dropped once the hedge
    // is decided, so the channel closes when every started arm has reported (or died).
    let start_hedge = |setup: &RaceSetup, tx: &tokio::sync::mpsc::Sender<RaceMsg>| -> bool {
        match setup.permits.try_acquire_hedge() {
            Some(p) => {
                setup.metrics.race_runs.fetch_add(1, Relaxed);
                spawn_race_arm(setup, hedge, 1, p, &claimed, tx);
                true
            }
            None => {
                setup.metrics.race_hedge_denied.fetch_add(1, Relaxed);
                false
            }
        }
    };
    let mut raced = false;
    let mut tx = Some(tx);
    let mut hedge_at: Option<tokio::time::Instant> = None;
    if delay.is_zero() {
        raced = start_hedge(&setup, tx.as_ref().expect("sender held until the hedge is decided"));
        tx = None;
    } else {
        hedge_at = Some(tokio::time::Instant::now() + delay);
    }

    let mut truncated: Option<(engine_adapter::SearchOutcome, Option<u32>, u64)> = None;
    loop {
        let msg = match hedge_at {
            Some(at) => tokio::select! {
                biased;
                m = rx.recv() => m,
                _ = tokio::time::sleep_until(at) => {
                    hedge_at = None;
                    if let Some(tx) = tx.take() {
                        raced = start_hedge(&setup, &tx);
                    }
                    continue;
                }
            },
            None => rx.recv().await,
        };
        match msg {
            Some(RaceMsg::Done(out)) => {
                if raced {
                    let ctr = if out.engine == "bidir" { &setup.metrics.race_wins_bidir } else { &setup.metrics.race_wins_uni };
                    ctr.fetch_add(1, Relaxed);
                } else if hedge_at.is_some() {
                    setup.metrics.race_hedge_skipped.fetch_add(1, Relaxed);
                }
                return Ok(out);
            }
            Some(RaceMsg::Truncated(o, ve, us)) => {
                // A stable loser also lands here (it lost the claim); the winner's Done
                // is already in the channel or arriving, so keep waiting until the
                // channel closes.
                truncated = Some(match truncated.take() {
                    None => (o, ve, us),
                    Some((po, pve, pus)) => {
                        if stable(&o) || (o.res.found && (!po.res.found || o.res.cost < po.res.cost)) { (o, ve, us) } else { (po, pve, pus) }
                    }
                });
                // The primary gave up (budget) inside the hedge delay: hedge now — the
                // other engine may prove the route within its own budget.
                if hedge_at.take().is_some() {
                    if let Some(tx) = tx.take() {
                        raced = start_hedge(&setup, &tx);
                    }
                }
            }
            None => break,
        }
    }
    let (o, ve, us) = truncated.ok_or_else(|| "race: no engine reported".to_string())?;
    let job = setup.job.clone();
    tokio::task::spawn_blocking(move || job.finish(o, ve, us)).await.map_err(|e| e.to_string())
}

#[derive(Debug, Serialize)]
pub struct ReloadResponse { pub reloaded: bool, pub snapshot_hash: Option<String>, pub loaded_at: u64 }

pub async fn reload(State(state): State<AppState>, Query(q): Query<ReloadQuery>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let cur = state.current.load_full();
    let path = cur.path.clone();
    let force = matches!(q.force.as_deref().map(str::trim), Some("1") | Some("true"));

    // Snapshot open + provider/component/canonical builds are ~100 ms of CPU work —
    // run them on the blocking pool so reactor threads keep serving requests.
    let log_path = path.clone();
    let serving = cur.clone();
    let built = tokio::task::spawn_blocking(move || {
        // The file on disk is the snapshot already serving (same blake3 tail hash): keep
        // the live state — mapping, derived structures and warm caches. `?force=1`
        // rebuilds anyway.
        if !force && serving.snapshot.is_some() {
            let disk = crate::read_tail_hash_hex(&path);
            if disk.is_some() && disk == serving.snapshot_hash_hex {
                return Ok(None);
            }
        }
        let new_snap = navpath_core::Snapshot::open(&path).map_err(|e| e.to_string())?;
        // Page the new mapping in BEFORE it is swapped live.
        let warm = crate::warm_snapshot(&new_snap);
        let new_hash = new_snap.tail_hash_hex();
        let st = SnapshotState::build(path, new_snap, new_hash);
        st.set_warm_state(warm);
        Ok::<Option<SnapshotState>, String>(Some(st))
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    match built {
        Ok(Some(new_state)) => {
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
        Ok(None) => {
            info!(path=?log_path, hash=?cur.snapshot_hash_hex, "snapshot unchanged on disk; reload skipped (?force=1 rebuilds)");
            Ok(Json(serde_json::json!({
                "reloaded": false,
                "unchanged": true,
                "snapshot_hash": cur.snapshot_hash_hex,
                "loaded_at": cur.loaded_at_unix
            })))
        }
        Err(e) => {
            warn!(error=%e, path=?log_path, "reload failed; keeping old snapshot");
            Err((StatusCode::CONFLICT, e))
        }
    }
}

/// `POST /admin/reload?force=1` rebuilds even when the snapshot file is unchanged.
#[derive(Debug, Deserialize, Default)]
pub struct ReloadQuery {
    #[serde(default)]
    pub force: Option<String>,
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
