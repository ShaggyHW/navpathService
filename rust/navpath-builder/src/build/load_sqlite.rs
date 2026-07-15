use anyhow::Result;
use rusqlite::{Connection, Row};

use super::graph::NodeIndex;

#[derive(Debug, Clone, Copy)]
pub struct Tile {
    pub x: i32,
    pub y: i32,
    pub plane: i32,
    pub walk_mask: u32,
}

/// A Fairy Ring node loaded from SQLite
#[derive(Debug, Clone)]
pub struct FairyRingRow {
    pub id: i64,
    pub node_id: u32,
    pub object_id: i64,
    pub x: i32,
    pub y: i32,
    pub plane: i32,
    pub cost: f32,
    pub code: String,
    pub action: Option<String>,
    pub next_node_type: Option<String>,
    pub next_node_id: Option<i64>,
    pub requirements: Vec<i64>,
}

/// Fast path: one row per (region, plane) with a presence bitmap + walk_mask blob
/// (see migrate_tiles_regions.py). ~2.8k rows and memcpy-speed decode instead of
/// millions of per-row varint decodes; essential at 4M tiles.
fn load_tiles_from_regions(conn: &Connection) -> Result<Option<Vec<Tile>>> {
    let has: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='tiles_regions'",
        [],
        |r| r.get(0),
    )?;
    if has == 0 {
        return Ok(None);
    }
    let mut stmt = conn.prepare("SELECT plane, base_x, base_y, blob FROM tiles_regions")?;
    let rows = stmt.query_map([], |row: &Row| {
        let plane: i32 = row.get(0)?;
        let base_x: i32 = row.get(1)?;
        let base_y: i32 = row.get(2)?;
        let blob: Vec<u8> = row.get(3)?;
        Ok((plane, base_x, base_y, blob))
    })?;
    let mut out = Vec::new();
    for r in rows {
        let (plane, base_x, base_y, blob) = r?;
        if blob.len() != 512 + 4096 {
            anyhow::bail!("tiles_regions blob has unexpected length {}", blob.len());
        }
        let (presence, masks) = blob.split_at(512);
        for i in 0..4096usize {
            if presence[i / 8] & (1 << (i % 8)) != 0 {
                out.push(Tile {
                    x: base_x + (i % 64) as i32,
                    y: base_y + (i / 64) as i32,
                    plane,
                    walk_mask: masks[i] as u32,
                });
            }
        }
    }
    Ok(Some(out))
}

pub fn load_all_tiles(conn: &Connection) -> Result<Vec<Tile>> {
    use rayon::prelude::*;
    if let Some(mut tiles) = load_tiles_from_regions(conn)? {
        tiles.par_sort_unstable_by_key(|t| (t.plane, t.y, t.x));
        return Ok(tiles);
    }
    // Loud, not silent: this fallback costs ~10x the region path (and ~200+ MB of DB
    // at 4M tiles). A DB without tiles_regions is a producer regression — re-run
    // migrate_tiles_regions.py (see docs/optimization_roadmap_v2.md §1.4).
    tracing::warn!(
        "tiles_regions table missing; falling back to the slow row-per-tile scan — \
         re-run migrate_tiles_regions.py against this DB"
    );
    // No SQL ORDER BY: the tiles PK is (x,y,plane), so ordering by (plane,y,x) forces a
    // TEMP B-TREE sort over every row (~15x slower scan). Load in table order and sort
    // in Rust below — node-id assignment stays byte-identical.
    let mut stmt = conn.prepare(
        "SELECT x, y, plane, walk_mask FROM tiles",
    )?;
    let rows = stmt.query_map([], |row: &Row| {
        let x: i32 = row.get(0)?;
        let y: i32 = row.get(1)?;
        let plane: i32 = row.get(2)?;
        let walk_mask_val: i64 = row.get(3)?; // SQLite uses INTEGER -> i64
        Ok(Tile {
            x,
            y,
            plane,
            walk_mask: walk_mask_val as u32,
        })
    })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    // Deterministic despite parallelism: (plane, y, x) keys are unique per tile.
    out.par_sort_unstable_by_key(|t| (t.plane, t.y, t.x));
    Ok(out)
}

/// Parse a requirements string (comma or semicolon separated) into a Vec<i64>
fn parse_requirements(reqs: Option<String>) -> Vec<i64> {
    let Some(s) = reqs else { return Vec::new(); };
    let s = s.trim();
    if s.is_empty() { return Vec::new(); }
    s.split(|c| c == ';' || c == ',')
        .filter_map(|part| {
            let p = part.trim();
            if p.is_empty() { return None; }
            p.parse::<i64>().ok()
        })
        .collect()
}

/// Load all Fairy Ring nodes from the teleports_fairy_rings_nodes table.
/// Skips rows whose coordinates are not present in the tiles table (fail-soft).
pub fn load_fairy_rings(
    conn: &Connection,
    node_id_of: &NodeIndex,
) -> Result<Vec<FairyRingRow>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT id, object_id, x, y, plane, cost, code, action, next_node_type, next_node_id, requirements
        FROM teleports_fairy_rings_nodes
        ORDER BY plane, y, x
        "#,
    )?;

    let rows = stmt.query_map([], |row: &Row| {
        let id: i64 = row.get(0)?;
        let object_id: i64 = row.get(1)?;
        let x: i32 = row.get(2)?;
        let y: i32 = row.get(3)?;
        let plane: i32 = row.get(4)?;
        let cost_f: f64 = row.get::<_, Option<f64>>(5)?.unwrap_or(0.0);
        let code: String = row.get::<_, Option<String>>(6)?.unwrap_or_default();
        let action: Option<String> = row.get(7)?;
        let next_node_type: Option<String> = row.get(8)?;
        let next_node_id: Option<i64> = row.get(9)?;
        let requirements_str: Option<String> = row.get(10)?;
        Ok((id, object_id, x, y, plane, cost_f as f32, code, action, next_node_type, next_node_id, requirements_str))
    })?;

    let mut out = Vec::new();
    for r in rows {
        let (id, object_id, x, y, plane, cost, code, action, next_node_type, next_node_id, requirements_str) = r?;
        // Skip if coordinate not in tiles (fail-soft)
        let Some(node_id) = node_id_of.get(x, y, plane) else {
            continue;
        };
        // Skip invalid costs
        let c = if cost.is_finite() && cost >= 0.0 { cost } else { 0.0 };
        let requirements = parse_requirements(requirements_str);
        out.push(FairyRingRow {
            id,
            node_id,
            object_id,
            x,
            y,
            plane,
            cost: c,
            code,
            action,
            next_node_type,
            next_node_id,
            requirements,
        });
    }
    Ok(out)
}
