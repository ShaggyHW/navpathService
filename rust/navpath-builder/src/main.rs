use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use rusqlite::{Connection, OpenFlags};
use tracing::{error, info};
use tracing_subscriber::FmtSubscriber;

use navpath_core::snapshot::write_snapshot;

mod build;
use build::graph::compile_walk_edges;
use build::load_sqlite::load_all_tiles;
use build::chains::{flatten_chains, flatten_global_chains};
use build::requirements::compile_requirement_tags;
use build::landmarks::compute_alt_tables;

#[derive(Parser, Debug)]
#[command(name = "navpath-builder", version, about = "Build RS3 pathfinding snapshot from worldReachableTiles.db")] 
struct Args {
    /// Path to worldReachableTiles.db
    #[arg(long = "sqlite", value_name = "PATH")] 
    sqlite_path: PathBuf,

    /// Output snapshot file
    #[arg(long = "out-snapshot", value_name = "PATH")] 
    out_snapshot: PathBuf,

    /// Optional tiles.bin output (compact walk flags)
    #[arg(long = "out-tiles", value_name = "PATH")] 
    out_tiles: Option<PathBuf>,

    /// Landmark count (simple selection for now)
    #[arg(long = "landmarks", value_name = "N", default_value_t = 0)] 
    landmarks: u32,
}

// Build a db_row JSON object for the first step of a macro-edge, depending on kind
fn fetch_db_row(conn: &Connection, kind: &str, id: i64) -> Option<serde_json::Value> {
    match kind {
        "door" => {
            if let Ok(mut st) = conn.prepare(
                "SELECT direction,
                        tile_inside_x, tile_inside_y, tile_inside_plane,
                        tile_outside_x, tile_outside_y, tile_outside_plane,
                        location_open_x, location_open_y, location_open_plane,
                        location_closed_x, location_closed_y, location_closed_plane,
                        real_id_open, real_id_closed,
                        open_action,
                        cost, next_node_type, next_node_id, requirement_id
                 FROM teleports_door_nodes WHERE id = ?1"
            ) {
                let row: std::result::Result<serde_json::Value, _> = st.query_row([id], |r: &rusqlite::Row| {
                    let dir: Option<String> = r.get(0)?;
                    let inx: Option<i64> = r.get(1)?; let iny: Option<i64> = r.get(2)?; let inp: Option<i64> = r.get(3)?;
                    let ox: Option<i64> = r.get(4)?; let oy: Option<i64> = r.get(5)?; let op: Option<i64> = r.get(6)?;
                    let lox: Option<i64> = r.get(7)?; let loy: Option<i64> = r.get(8)?; let lop: Option<i64> = r.get(9)?;
                    let lcx: Option<i64> = r.get(10)?; let lcy: Option<i64> = r.get(11)?; let lcp: Option<i64> = r.get(12)?;
                    let rid_open: Option<i64> = r.get(13)?; let rid_closed: Option<i64> = r.get(14)?;
                    let open_action: Option<String> = r.get(15)?;
                    let cost: Option<f64> = r.get(16)?; let next_t: Option<String> = r.get(17)?; let next_id: Option<i64> = r.get(18)?; let req: Option<i64> = r.get(19)?;
                    let mut obj = serde_json::Map::new();
                    obj.insert("direction".to_string(), dir.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    obj.insert("tile_inside".to_string(), match (inx,iny,inp) { (Some(x),Some(y),Some(p)) => serde_json::json!([x as i32,y as i32,p as i32]), _ => serde_json::Value::Null });
                    obj.insert("tile_outside".to_string(), match (ox,oy,op) { (Some(x),Some(y),Some(p)) => serde_json::json!([x as i32,y as i32,p as i32]), _ => serde_json::Value::Null });
                    obj.insert("location_open".to_string(), match (lox,loy,lop) { (Some(x),Some(y),Some(p)) => serde_json::json!([x as i32,y as i32,p as i32]), _ => serde_json::Value::Null });
                    obj.insert("location_closed".to_string(), match (lcx,lcy,lcp) { (Some(x),Some(y),Some(p)) => serde_json::json!([x as i32,y as i32,p as i32]), _ => serde_json::Value::Null });
                    obj.insert("real_id_open".to_string(), rid_open.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("real_id_closed".to_string(), rid_closed.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("cost".to_string(), cost.map(|c| serde_json::Value::from(c as f32)).unwrap_or(serde_json::Value::Null));
                    obj.insert("open_action".to_string(), open_action.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    obj.insert("next_node_type".to_string(), next_t.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    obj.insert("next_node_id".to_string(), next_id.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("requirement_id".to_string(), req.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    Ok(serde_json::Value::Object(obj))
                });
                return row.ok();
            }
            None
        }
        "lodestone" => {
            if let Ok(mut st) = conn.prepare(
                "SELECT lodestone, dest_x, dest_y, dest_plane, cost, next_node_type, next_node_id, requirement_id
                 FROM teleports_lodestone_nodes WHERE id = ?1"
            ) {
                let row: std::result::Result<serde_json::Value, _> = st.query_row([id], |r: &rusqlite::Row| {
                    let name: Option<String> = r.get(0)?;
                    let dx: Option<i64> = r.get(1)?; let dy: Option<i64> = r.get(2)?; let dp: Option<i64> = r.get(3)?;
                    let cost: Option<f64> = r.get(4)?; let next_t: Option<String> = r.get(5)?; let next_id: Option<i64> = r.get(6)?; let req: Option<i64> = r.get(7)?;
                    let mut obj = serde_json::Map::new();
                    if let Some(s) = name { obj.insert("lodestone".to_string(), serde_json::Value::String(s)); }
                    obj.insert("dest".to_string(), match (dx,dy,dp) { (Some(x),Some(y),Some(p)) => serde_json::json!([x as i32,y as i32,p as i32]), _ => serde_json::Value::Null });
                    obj.insert("cost".to_string(), cost.map(|c| serde_json::Value::from(c as f32)).unwrap_or(serde_json::Value::Null));
                    obj.insert("next_node_type".to_string(), next_t.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    obj.insert("next_node_id".to_string(), next_id.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("requirement_id".to_string(), req.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    Ok(serde_json::Value::Object(obj))
                });
                return row.ok();
            }
            None
        }
        "object" => {
            if let Ok(mut st) = conn.prepare(
                "SELECT match_type, object_id, object_name, action,
                        dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane,
                        orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane,
                        search_radius,
                        cost, next_node_type, next_node_id, requirement_id
                 FROM teleports_object_nodes WHERE id = ?1"
            ) {
                let row: std::result::Result<serde_json::Value, _> = st.query_row([id], |r: &rusqlite::Row| {
                    let mt: Option<String> = r.get(0)?; let oid: Option<i64> = r.get(1)?; let oname: Option<String> = r.get(2)?; let action: Option<String> = r.get(3)?;
                    let dminx: Option<i64> = r.get(4)?; let dmaxx: Option<i64> = r.get(5)?; let dminy: Option<i64> = r.get(6)?; let dmaxy: Option<i64> = r.get(7)?; let dp: Option<i64> = r.get(8)?;
                    let ominx: Option<i64> = r.get(9)?; let omaxx: Option<i64> = r.get(10)?; let ominy: Option<i64> = r.get(11)?; let omaxy: Option<i64> = r.get(12)?; let op: Option<i64> = r.get(13)?;
                    let sr: Option<i64> = r.get(14)?;
                    let cost: Option<f64> = r.get(15)?; let next_t: Option<String> = r.get(16)?; let next_id: Option<i64> = r.get(17)?; let req: Option<i64> = r.get(18)?;
                    let mut obj = serde_json::Map::new();
                    obj.insert("match_type".to_string(), mt.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    obj.insert("object_id".to_string(), oid.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("object_name".to_string(), oname.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    obj.insert("action".to_string(), action.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    obj.insert("dest_min_x".to_string(), dminx.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("dest_max_x".to_string(), dmaxx.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("dest_min_y".to_string(), dminy.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("dest_max_y".to_string(), dmaxy.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("dest_plane".to_string(), dp.map(|v| serde_json::Value::from(v as i32)).unwrap_or(serde_json::Value::Null));
                    obj.insert("orig_min_x".to_string(), ominx.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("orig_max_x".to_string(), omaxx.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("orig_min_y".to_string(), ominy.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("orig_max_y".to_string(), omaxy.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("orig_plane".to_string(), op.map(|v| serde_json::Value::from(v as i32)).unwrap_or(serde_json::Value::Null));
                    obj.insert("search_radius".to_string(), sr.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("cost".to_string(), cost.map(|c| serde_json::Value::from(c as f32)).unwrap_or(serde_json::Value::Null));
                    obj.insert("next_node_type".to_string(), next_t.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    obj.insert("next_node_id".to_string(), next_id.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("requirement_id".to_string(), req.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    Ok(serde_json::Value::Object(obj))
                });
                return row.ok();
            }
            None
        }
        "npc" => {
            if let Ok(mut st) = conn.prepare(
                "SELECT match_type, npc_id, npc_name, action,
                        dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane,
                        orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane,
                        search_radius,
                        cost, next_node_type, next_node_id, requirement_id
                 FROM teleports_npc_nodes WHERE id = ?1"
            ) {
                let row: std::result::Result<serde_json::Value, _> = st.query_row([id], |r: &rusqlite::Row| {
                    let mt: Option<String> = r.get(0)?; let nid: Option<i64> = r.get(1)?; let nname: Option<String> = r.get(2)?; let action: Option<String> = r.get(3)?;
                    let dminx: Option<i64> = r.get(4)?; let dmaxx: Option<i64> = r.get(5)?; let dminy: Option<i64> = r.get(6)?; let dmaxy: Option<i64> = r.get(7)?; let dp: Option<i64> = r.get(8)?;
                    let ominx: Option<i64> = r.get(9)?; let omaxx: Option<i64> = r.get(10)?; let ominy: Option<i64> = r.get(11)?; let omaxy: Option<i64> = r.get(12)?; let op: Option<i64> = r.get(13)?;
                    let sr: Option<i64> = r.get(14)?;
                    let cost: Option<f64> = r.get(15)?; let next_t: Option<String> = r.get(16)?; let next_id: Option<i64> = r.get(17)?; let req: Option<i64> = r.get(18)?;
                    let mut obj = serde_json::Map::new();
                    obj.insert("match_type".to_string(), mt.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    obj.insert("npc_id".to_string(), nid.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("npc_name".to_string(), nname.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    obj.insert("action".to_string(), action.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    obj.insert("dest_min_x".to_string(), dminx.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("dest_max_x".to_string(), dmaxx.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("dest_min_y".to_string(), dminy.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("dest_max_y".to_string(), dmaxy.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("dest_plane".to_string(), dp.map(|v| serde_json::Value::from(v as i32)).unwrap_or(serde_json::Value::Null));
                    obj.insert("orig_min_x".to_string(), ominx.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("orig_max_x".to_string(), omaxx.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("orig_min_y".to_string(), ominy.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("orig_max_y".to_string(), omaxy.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("orig_plane".to_string(), op.map(|v| serde_json::Value::from(v as i32)).unwrap_or(serde_json::Value::Null));
                    obj.insert("search_radius".to_string(), sr.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("cost".to_string(), cost.map(|c| serde_json::Value::from(c as f32)).unwrap_or(serde_json::Value::Null));
                    obj.insert("next_node_type".to_string(), next_t.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    obj.insert("next_node_id".to_string(), next_id.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("requirement_id".to_string(), req.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    Ok(serde_json::Value::Object(obj))
                });
                return row.ok();
            }
            None
        }
        "item" => {
            if let Ok(mut st) = conn.prepare(
                "SELECT item_id, action,
                        dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane,
                        cost, next_node_type, next_node_id, requirement_id
                 FROM teleports_item_nodes WHERE id = ?1"
            ) {
                let row: std::result::Result<serde_json::Value, _> = st.query_row([id], |r: &rusqlite::Row| {
                    let iid: Option<i64> = r.get(0)?; let action: Option<String> = r.get(1)?;
                    let dminx: Option<i64> = r.get(2)?; let dmaxx: Option<i64> = r.get(3)?; let dminy: Option<i64> = r.get(4)?; let dmaxy: Option<i64> = r.get(5)?; let dp: Option<i64> = r.get(6)?;
                    let cost: Option<f64> = r.get(7)?; let next_t: Option<String> = r.get(8)?; let next_id: Option<i64> = r.get(9)?; let req: Option<i64> = r.get(10)?;
                    let mut obj = serde_json::Map::new();
                    obj.insert("item_id".to_string(), iid.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("action".to_string(), action.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    obj.insert("dest_min_x".to_string(), dminx.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("dest_max_x".to_string(), dmaxx.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("dest_min_y".to_string(), dminy.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("dest_max_y".to_string(), dmaxy.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("dest_plane".to_string(), dp.map(|v| serde_json::Value::from(v as i32)).unwrap_or(serde_json::Value::Null));
                    obj.insert("cost".to_string(), cost.map(|c| serde_json::Value::from(c as f32)).unwrap_or(serde_json::Value::Null));
                    obj.insert("next_node_type".to_string(), next_t.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    obj.insert("next_node_id".to_string(), next_id.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("requirement_id".to_string(), req.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    Ok(serde_json::Value::Object(obj))
                });
                return row.ok();
            }
            None
        }
        "ifslot" => {
            if let Ok(mut st) = conn.prepare(
                "SELECT interface_id, component_id, slot_id, click_id,
                        dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane,
                        cost, next_node_type, next_node_id, requirement_id
                 FROM teleports_ifslot_nodes WHERE id = ?1"
            ) {
                let row: std::result::Result<serde_json::Value, _> = st.query_row([id], |r: &rusqlite::Row| {
                    let iface: Option<i64> = r.get(0)?; let comp: Option<i64> = r.get(1)?; let slot: Option<i64> = r.get(2)?; let click: Option<i64> = r.get(3)?;
                    let dminx: Option<i64> = r.get(4)?; let dmaxx: Option<i64> = r.get(5)?; let dminy: Option<i64> = r.get(6)?; let dmaxy: Option<i64> = r.get(7)?; let dp: Option<i64> = r.get(8)?;
                    let cost: Option<f64> = r.get(9)?; let next_t: Option<String> = r.get(10)?; let next_id: Option<i64> = r.get(11)?; let req: Option<i64> = r.get(12)?;
                    let mut obj = serde_json::Map::new();
                    obj.insert("interface_id".to_string(), iface.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("component_id".to_string(), comp.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("slot_id".to_string(), slot.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("click_id".to_string(), click.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("dest_min_x".to_string(), dminx.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("dest_max_x".to_string(), dmaxx.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("dest_min_y".to_string(), dminy.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("dest_max_y".to_string(), dmaxy.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("dest_plane".to_string(), dp.map(|v| serde_json::Value::from(v as i32)).unwrap_or(serde_json::Value::Null));
                    obj.insert("cost".to_string(), cost.map(|c| serde_json::Value::from(c as f32)).unwrap_or(serde_json::Value::Null));
                    obj.insert("next_node_type".to_string(), next_t.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    obj.insert("next_node_id".to_string(), next_id.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("requirement_id".to_string(), req.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    Ok(serde_json::Value::Object(obj))
                });
                return row.ok();
            }
            None
        }
        _ => None,
    }
}

fn open_read_only(sqlite_path: &PathBuf) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        sqlite_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    // Best-effort pragmas; ignore failures
    let _ = conn.execute_batch(
        r#"
        PRAGMA query_only=ON;
        PRAGMA foreign_keys=OFF;
        PRAGMA journal_mode=OFF;
        PRAGMA synchronous=OFF;
        "#,
    );
    Ok(conn)
}

fn main() -> Result<()> {
    let subscriber = FmtSubscriber::builder().with_ansi(false).json().finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    let args = Args::parse();
    info!(?args, "starting builder");

    let conn = open_read_only(&args.sqlite_path)
        .with_context(|| format!("failed to open {:?}", args.sqlite_path))?;

    // Load tiles
    let tiles = load_all_tiles(&conn)?;
    if tiles.is_empty() {
        anyhow::bail!("no tiles found in DB");
    }

    // Map (x,y,plane) -> node id (u32)
    let mut node_id_of: HashMap<(i32, i32, i32), u32> = HashMap::with_capacity(tiles.len());
    for (i, t) in tiles.iter().enumerate() {
        node_id_of.insert((t.x, t.y, t.plane), i as u32);
    }

    // Nodes ids are sequential 0..n-1 for now; also collect coordinates
    let nodes_ids: Vec<u32> = (0..tiles.len() as u32).collect();
    let nodes_x: Vec<i32> = tiles.iter().map(|t| t.x).collect();
    let nodes_y: Vec<i32> = tiles.iter().map(|t| t.y).collect();
    let nodes_plane: Vec<i32> = tiles.iter().map(|t| t.plane).collect();

    // Compile walk edges with diagonal/cardinal rules
    let walk = compile_walk_edges(&tiles, &node_id_of);
    let (walk_src, walk_dst, walk_w) = walk;

    // Flatten chains into macro-edges with cycle detection and deterministic ordering
    let metas = flatten_chains(&conn, &tiles, &node_id_of)?;
    let mut macro_src = Vec::with_capacity(metas.len());
    let mut macro_dst = Vec::with_capacity(metas.len());
    let mut macro_w = Vec::with_capacity(metas.len());
    let mut macro_kind_first: Vec<u32> = Vec::with_capacity(metas.len());
    let mut macro_id_first: Vec<u32> = Vec::with_capacity(metas.len());
    let mut macro_meta_offs: Vec<u32> = Vec::with_capacity(metas.len());
    let mut macro_meta_lens: Vec<u32> = Vec::with_capacity(metas.len());
    let mut macro_meta_blob: Vec<u8> = Vec::new();
    for m in metas {
        macro_src.push(m.src);
        macro_dst.push(m.dst);
        macro_w.push(m.cost);
        // encode first step kind/id (0 if none)
        let (k, id) = if let Some(first) = m.steps.first() {
            let code = match first.kind {
                "door" => 1u32,
                "lodestone" => 2u32,
                "npc" => 3u32,
                "object" => 4u32,
                "item" => 5u32,
                "ifslot" => 6u32,
                _ => 0u32,
            };
            let idu = if first.id >= 0 { (first.id as u64).min(u32::MAX as u64) as u32 } else { 0u32 };
            (code, idu)
        } else { (0u32, 0u32) };
        macro_kind_first.push(k);
        macro_id_first.push(id);
        // Build compact metadata JSON per edge; can be extended without changing binary layout
        // Build steps with optional lodestone name and best-effort db_row for each step
        let steps_json: Vec<serde_json::Value> = m.steps.iter().map(|s| {
            let mut obj = serde_json::Map::new();
            obj.insert("kind".to_string(), serde_json::Value::String(s.kind.to_string()));
            obj.insert("id".to_string(), serde_json::Value::from(s.id));
            obj.insert("cost_ms".to_string(), serde_json::Value::from(s.cost));
            if let Some(ref name) = s.lodestone {
                obj.insert("lodestone".to_string(), serde_json::Value::String(name.clone()));
            }
            // Best-effort: include the raw DB row for this specific step if available
            if let Some(mut v) = fetch_db_row(&conn, s.kind, s.id as i64) {
                // If present, also attach one-level deep next node db_row for convenience
                let next_t = v.get("next_node_type").and_then(|x| x.as_str()).map(|s| s.to_string());
                let next_id = v.get("next_node_id").and_then(|x| x.as_i64());
                if let (Some(t), Some(n)) = (next_t, next_id) {
                    if let Some(next_v) = fetch_db_row(&conn, &t, n) {
                        if let Some(map) = v.as_object_mut() {
                            map.insert("next_db_row".to_string(), next_v);
                        }
                    }
                }
                obj.insert("db_row".to_string(), v);
            }
            serde_json::Value::Object(obj)
        }).collect();

        // Start building meta object
        let mut meta_obj = serde_json::Map::new();
        meta_obj.insert("kind".to_string(), serde_json::Value::String(match k { 1=>"door",2=>"lodestone",3=>"npc",4=>"object",5=>"item",6=>"ifslot", _=>"unknown" }.to_string()));
        meta_obj.insert("first_id".to_string(), serde_json::Value::from(id));
        meta_obj.insert("steps".to_string(), serde_json::Value::from(steps_json));
        meta_obj.insert("requirements".to_string(), serde_json::Value::from(m.requirement_ids.clone()));

        // Best-effort: fetch db_row for the first step's kind/id for richer metadata on all teleport kinds
        if let Some(first) = m.steps.first() {
            let kind_str = first.kind;
            let fid = first.id as i64;
            if let Some(mut v) = fetch_db_row(&conn, kind_str, fid) {
                // Attach shallow next_db_row if available
                let next_t = v.get("next_node_type").and_then(|x| x.as_str()).map(|s| s.to_string());
                let next_id = v.get("next_node_id").and_then(|x| x.as_i64());
                if let (Some(t), Some(n)) = (next_t, next_id) {
                    if let Some(next_v) = fetch_db_row(&conn, &t, n) {
                        if let Some(map) = v.as_object_mut() {
                            map.insert("next_db_row".to_string(), next_v);
                        }
                    }
                }
                meta_obj.insert("db_row".to_string(), v);
            }
        }

        let meta = serde_json::Value::Object(meta_obj);
        let bytes = serde_json::to_vec(&meta).unwrap_or_else(|_| b"{}".to_vec());
        let off = macro_meta_blob.len() as u32;
        macro_meta_offs.push(off);
        macro_meta_lens.push(bytes.len() as u32);
        macro_meta_blob.extend_from_slice(&bytes);
    }

    // Global teleports (no concrete source): encode once in metadata under a dummy macro edge 0->0
    // Service will attach them as extra edges from the current start node at query time.
    let gmetas = flatten_global_chains(&conn, &node_id_of)?;
    if !gmetas.is_empty() {
        macro_src.push(0);
        macro_dst.push(0);
        macro_w.push(f32::INFINITY);
        macro_kind_first.push(0);
        macro_id_first.push(0);
        let gmeta = {
            let arr: Vec<serde_json::Value> = gmetas.iter().map(|g| {
                let steps_json: Vec<serde_json::Value> = g.steps.iter().map(|s| {
                    let mut obj = serde_json::Map::new();
                    obj.insert("kind".to_string(), serde_json::Value::String(s.kind.to_string()));
                    obj.insert("id".to_string(), serde_json::Value::from(s.id));
                    obj.insert("cost_ms".to_string(), serde_json::Value::from(s.cost));
                    if let Some(ref name) = s.lodestone {
                        obj.insert("lodestone".to_string(), serde_json::Value::String(name.clone()));
                    }
                    // Best-effort: include the raw DB row for this specific step if available
                    if let Some(mut v) = fetch_db_row(&conn, s.kind, s.id as i64) {
                        let next_t = v.get("next_node_type").and_then(|x| x.as_str()).map(|s| s.to_string());
                        let next_id = v.get("next_node_id").and_then(|x| x.as_i64());
                        if let (Some(t), Some(n)) = (next_t, next_id) {
                            if let Some(next_v) = fetch_db_row(&conn, &t, n) {
                                if let Some(map) = v.as_object_mut() {
                                    map.insert("next_db_row".to_string(), next_v);
                                }
                            }
                        }
                        obj.insert("db_row".to_string(), v);
                    }
                    serde_json::Value::Object(obj)
                }).collect();
                let mut obj = serde_json::Map::new();
                obj.insert("dst".to_string(), serde_json::Value::from(g.dst));
                obj.insert("cost_ms".to_string(), serde_json::Value::from(g.cost));
                obj.insert("requirements".to_string(), serde_json::Value::from(g.requirement_ids.clone()));
                obj.insert("steps".to_string(), serde_json::Value::from(steps_json));
                // db_row for the first step of this global chain
                if let Some(first) = g.steps.first() {
                    let kind_str = first.kind;
                    let fid = first.id as i64;
                    if let Some(v) = fetch_db_row(&conn, kind_str, fid) {
                        obj.insert("db_row".to_string(), v);
                    }
                }
                serde_json::Value::Object(obj)
            }).collect();
            serde_json::json!({"global": arr})
        };
        let bytes = serde_json::to_vec(&gmeta).unwrap_or_else(|_| b"{}".to_vec());
        let off = macro_meta_blob.len() as u32;
        macro_meta_offs.push(off);
        macro_meta_lens.push(bytes.len() as u32);
        macro_meta_blob.extend_from_slice(&bytes);
    }

    // Compile requirement tags from teleports_requirements
    let req_tags: Vec<u32> = compile_requirement_tags(&conn)?;

    // Landmarks: pick first N nodes if requested
    let mut landmarks: Vec<u32> = Vec::new();
    if args.landmarks > 0 {
        let n = args.landmarks.min(nodes_ids.len() as u32) as usize;
        landmarks.extend((0..n as u32).into_iter());
    }

    // ALT tables (forward: LM->node, backward: node->LM via reverse graph)
    let (lm_fw, lm_bw) = if !landmarks.is_empty() {
        compute_alt_tables(
            nodes_ids.len(),
            &walk_src, &walk_dst, &walk_w,
            &macro_src, &macro_dst, &macro_w,
            &landmarks,
        )
    } else { (Vec::new(), Vec::new()) };

    // Write snapshot
    let res = write_snapshot(
        &args.out_snapshot,
        &nodes_ids,
        &nodes_x,
        &nodes_y,
        &nodes_plane,
        &walk_src,
        &walk_dst,
        &walk_w,
        &macro_src,
        &macro_dst,
        &macro_w,
        &macro_kind_first,
        &macro_id_first,
        &macro_meta_offs,
        &macro_meta_lens,
        &macro_meta_blob,
        &req_tags,
        &landmarks,
        &lm_fw,
        &lm_bw,
    );

    match res {
        Ok(info) => {
            info!(manifest = ?info.manifest, hash = ?info.hash, "wrote snapshot");
        }
        Err(e) => {
            error!(error = ?e, "failed to write snapshot");
            return Err(e.into());
        }
    }

    // Optionally write tiles.bin (compact walk flags order matches nodes_ids)
    if let Some(out_tiles) = args.out_tiles {
        let mut f = File::create(&out_tiles)
            .with_context(|| format!("creating {:?}", out_tiles))?;
        for t in &tiles {
            let b = (t.walk_mask & 0xFF) as u8;
            f.write_all(&[b])?;
        }
        f.flush()?;
        info!(path = ?out_tiles, bytes = tiles.len(), "wrote tiles.bin");
    }

    Ok(())
}
