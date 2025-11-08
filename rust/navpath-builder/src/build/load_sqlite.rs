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
