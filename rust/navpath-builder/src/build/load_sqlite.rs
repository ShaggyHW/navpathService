use std::collections::HashMap;

use anyhow::Result;
use rusqlite::{Connection, Row};

#[derive(Debug, Clone, Copy)]
pub struct Tile {
    pub x: i32,
    pub y: i32,
    pub plane: i32,
    pub walk_mask: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct TeleEdge {
    pub src: u32,
    pub dst: u32,
    pub cost: f32,
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

pub fn load_all_tiles(conn: &Connection) -> Result<Vec<Tile>> {
    let mut stmt = conn.prepare(
        "SELECT x, y, plane, walk_mask FROM tiles ORDER BY plane, y, x",
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
            walk_mask: (walk_mask_val as i128 as i64) as u32, // truncate to 32 bits safely
        })
    })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub fn load_localized_teleports(
    conn: &Connection,
    node_id_of: &HashMap<(i32, i32, i32), u32>,
) -> Result<Vec<TeleEdge>> {
    // Only rows with concrete source and destination
    let mut stmt = conn.prepare(
        r#"
        SELECT src_x, src_y, src_plane, dst_x, dst_y, dst_plane, cost
        FROM teleports_all
        WHERE src_x IS NOT NULL AND src_y IS NOT NULL AND src_plane IS NOT NULL
          AND dst_x IS NOT NULL AND dst_y IS NOT NULL AND dst_plane IS NOT NULL
        ORDER BY src_plane, src_y, src_x
        "#,
    )?;

    let rows = stmt.query_map([], |row: &Row| {
        let sx: i32 = row.get(0)?;
        let sy: i32 = row.get(1)?;
        let sp: i32 = row.get(2)?;
        let dx: i32 = row.get(3)?;
        let dy: i32 = row.get(4)?;
        let dp: i32 = row.get(5)?;
        let cost_f: f64 = row.get(6)?;
        Ok(((sx, sy, sp), (dx, dy, dp), cost_f as f32))
    })?;

    let mut out = Vec::new();
    for r in rows {
        let ((sx, sy, sp), (dx, dy, dp), cost) = r?;
        if let (Some(&s), Some(&d)) = (
            node_id_of.get(&(sx, sy, sp)),
            node_id_of.get(&(dx, dy, dp)),
        ) {
            // Skip obviously invalid costs
            let c = if cost.is_finite() && cost >= 0.0 { cost } else { 0.0 };
            out.push(TeleEdge { src: s, dst: d, cost: c });
        }
    }
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
    node_id_of: &HashMap<(i32, i32, i32), u32>,
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
        let Some(&node_id) = node_id_of.get(&(x, y, plane)) else {
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
