//! Danger areas: rectangles the router walks around unless entering them is strictly
//! required (requirement `avoidDangerAreas`, default on).
//!
//! File format (like the rs3cache extractor's `override.txt`): one area per line,
//! comma-separated, inclusive bounds
//!
//! ```text
//! # minx, maxx, miny, maxy[, plane]
//! 2944, 3392, 3520, 3968        # all planes
//! 3200, 3210, 3200, 3210, 0     # plane 0 only
//! ```
//!
//! Blank lines and `#` comments are ignored; swapped min/max are normalized. A bad line
//! is logged and skipped (a typo must not take the router down).
//!
//! Location: `NAVPATH_DANGER_AREAS` (path), default `dangerAreas.txt` next to the
//! snapshot. A missing file means no danger areas. The file is read at startup and on
//! every `POST /admin/reload`.
//!
//! Penalty: every danger tile a route ENTERS costs `NAVPATH_DANGER_PENALTY_MS` extra
//! (default 1,000,000 ms ≈ 16.7 min of travel per tile) during the search; the served
//! cost never includes it. The route therefore takes any detour shorter than that per
//! avoided tile, and when the start, the goal, or a chokepoint forces it inside, it uses
//! as few danger tiles as possible.

use std::path::{Path, PathBuf};

use navpath_core::snapshot::unpack_coord;
use navpath_core::{NodePenalty, Snapshot};
use tracing::{info, warn};

/// Default per-tile penalty (ms) — see the module docs.
pub const DEFAULT_DANGER_PENALTY_MS: f32 = 1_000_000.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DangerArea {
    pub min_x: i32,
    pub max_x: i32,
    pub min_y: i32,
    pub max_y: i32,
    /// None = every plane.
    pub plane: Option<i32>,
}

impl DangerArea {
    #[inline]
    pub fn contains(&self, x: i32, y: i32, plane: i32) -> bool {
        x >= self.min_x && x <= self.max_x && y >= self.min_y && y <= self.max_y && self.plane.is_none_or(|p| p == plane)
    }
}

/// Parse a danger-areas file body. Returns the areas plus one message per skipped line.
pub fn parse_danger_areas(text: &str) -> (Vec<DangerArea>, Vec<String>) {
    let mut areas = Vec::new();
    let mut errors = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split(',').map(str::trim).collect();
        if parts.len() != 4 && parts.len() != 5 {
            errors.push(format!("line {line_no}: expected minx,maxx,miny,maxy[,plane], got {} values", parts.len()));
            continue;
        }
        let nums: Result<Vec<i32>, _> = parts.iter().map(|p| p.parse::<i32>()).collect();
        let Ok(nums) = nums else {
            errors.push(format!("line {line_no}: not an integer in `{line}`"));
            continue;
        };
        let plane = nums.get(4).copied();
        if plane.is_some_and(|p| !(0..4).contains(&p)) {
            errors.push(format!("line {line_no}: plane must be 0..=3"));
            continue;
        }
        areas.push(DangerArea {
            min_x: nums[0].min(nums[1]),
            max_x: nums[0].max(nums[1]),
            min_y: nums[2].min(nums[3]),
            max_y: nums[2].max(nums[3]),
            plane,
        });
    }
    (areas, errors)
}

/// Danger areas resolved against one snapshot: a node-id bitmap of the flagged tiles.
pub struct DangerAreas {
    pub path: PathBuf,
    pub areas: Vec<DangerArea>,
    /// One bit per node id (see [`NodePenalty::bits`]).
    pub bits: Vec<u64>,
    /// Number of snapshot tiles inside some area.
    pub tiles: u32,
    pub penalty_ms: f32,
}

impl DangerAreas {
    pub fn build(snap: &Snapshot, path: PathBuf, areas: Vec<DangerArea>, penalty_ms: f32) -> DangerAreas {
        let coords = snap.coords_packed();
        let mut bits = vec![0u64; coords.len().div_ceil(64)];
        let mut tiles = 0u32;
        if !areas.is_empty() {
            for (v, &c) in coords.iter().enumerate() {
                let (x, y, p) = unpack_coord(c);
                if areas.iter().any(|a| a.contains(x, y, p)) {
                    bits[v >> 6] |= 1 << (v & 63);
                    tiles += 1;
                }
            }
        }
        DangerAreas { path, areas, bits, tiles, penalty_ms }
    }

    /// Read the configured file (see the module docs) and resolve it against `snap`.
    /// A missing file yields an empty set.
    pub fn load(snap: &Snapshot, snapshot_path: &Path) -> DangerAreas {
        let path = danger_areas_path(snapshot_path);
        let penalty_ms = danger_penalty_ms();
        let areas = match std::fs::read_to_string(&path) {
            Ok(text) => {
                let (areas, errors) = parse_danger_areas(&text);
                for e in &errors {
                    warn!(path = ?path, "danger areas: skipped {e}");
                }
                areas
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info!(path = ?path, "no danger-areas file; danger avoidance inactive");
                Vec::new()
            }
            Err(e) => {
                warn!(path = ?path, error = %e, "could not read danger-areas file; danger avoidance inactive");
                Vec::new()
            }
        };
        let t = std::time::Instant::now();
        let d = DangerAreas::build(snap, path, areas, penalty_ms);
        if !d.areas.is_empty() {
            info!(path = ?d.path, areas = d.areas.len(), tiles = d.tiles, penalty_ms, elapsed_ms = t.elapsed().as_millis() as u64, "loaded danger areas");
        }
        d
    }

    /// No snapshot tile is flagged: avoidance is a no-op.
    pub fn is_empty(&self) -> bool {
        self.tiles == 0
    }

    pub fn penalty(&self) -> NodePenalty<'_> {
        NodePenalty { bits: &self.bits, ms: self.penalty_ms }
    }

    #[inline]
    pub fn contains(&self, v: u32) -> bool {
        self.penalty().contains(v)
    }

    /// Whether `path` enters a danger tile. `origin_first`: `path[0]` is the request's
    /// on-graph start, which is never "entered" (a virtual start's `path[0]` is a
    /// teleport landing, which is).
    pub fn path_enters(&self, path: &[u32], origin_first: bool) -> bool {
        let skip = usize::from(origin_first);
        path.iter().skip(skip).any(|&v| self.contains(v))
    }
}

pub fn danger_areas_path(snapshot_path: &Path) -> PathBuf {
    if let Ok(p) = std::env::var("NAVPATH_DANGER_AREAS") {
        if !p.trim().is_empty() {
            return PathBuf::from(p.trim());
        }
    }
    snapshot_path.parent().map(|d| d.join("dangerAreas.txt")).unwrap_or_else(|| PathBuf::from("dangerAreas.txt"))
}

fn danger_penalty_ms() -> f32 {
    std::env::var("NAVPATH_DANGER_PENALTY_MS")
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(DEFAULT_DANGER_PENALTY_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_override_style_lines() {
        let text = "# wilderness\n2944, 3392, 3520, 3968\n\n3210,3200, 3210,3200 ,0  # swapped\n1,2,3\n1,2,x,4\n1,2,3,4,7\n";
        let (areas, errors) = parse_danger_areas(text);
        assert_eq!(
            areas,
            vec![
                DangerArea { min_x: 2944, max_x: 3392, min_y: 3520, max_y: 3968, plane: None },
                DangerArea { min_x: 3200, max_x: 3210, min_y: 3200, max_y: 3210, plane: Some(0) },
            ]
        );
        assert_eq!(errors.len(), 3);
        assert!(areas[1].contains(3200, 3210, 0));
        assert!(!areas[1].contains(3200, 3210, 1));
        assert!(areas[0].contains(3000, 3600, 2));
    }
}
