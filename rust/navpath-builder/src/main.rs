use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use rusqlite::{Connection, OpenFlags};
use tracing::{error, info};
use tracing_subscriber::FmtSubscriber;

use navpath_core::snapshot::{pack_coord, write_snapshot, AltFormat, SnapshotSections, WriteOptions};

use navpath_builder::build;
use build::chains::flatten_all_chains;
use build::components::{walk_components, Structure};
use build::dijkstra::AltGraph;
use build::graph::compile_walk_csr;
use build::landmarks::{align_landmark_count, build_alt, table_stats, AltConfig, Strategy};
use build::load_sqlite::{load_all_tiles, load_fairy_rings};
use build::requirements::compile_requirement_tags;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

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

    /// walkableTiles.bin output: a coordinate-keyed presence bitmap of every tile in the
    /// DB (the same set `/tile/exists` answers from), small enough to ship inside a client.
    /// Defaults to `walkableTiles.bin` next to the snapshot; `--no-walkable` skips it.
    #[arg(long = "out-walkable", value_name = "PATH")] 
    out_walkable: Option<PathBuf>,

    /// Skip writing walkableTiles.bin
    #[arg(long = "no-walkable", default_value_t = false)] 
    no_walkable: bool,

    /// Landmark count (0 = no ALT table). Rounded UP to a multiple of 16: the runtime's
    /// AVX-512 full-row heuristic needs a row stride (2 u16 per landmark) that is a
    /// whole number of 32-lane registers.
    #[arg(long = "landmarks", value_name = "N", default_value_t = 0)]
    landmarks: u32,

    /// Landmark placement: `scc` (default) splits the budget across strongly connected
    /// components and places landmarks by symmetric farthest-point inside each; `legacy`
    /// reproduces the old forward-only farthest-point selection byte for byte (A/B).
    #[arg(long = "landmark-strategy", value_enum, default_value_t = LandmarkStrategy::Scc)]
    landmark_strategy: LandmarkStrategy,

    /// SCC strategy only: give every weakly disconnected component local landmarks,
    /// written into the main landmarks' columns (no extra table bytes).
    #[arg(long = "local-fill", default_value_t = true, action = clap::ArgAction::Set)]
    local_fill: bool,

    /// ALT table encoding. `packed` (default): per-16-node-cluster u16 bases plus u8
    /// offsets, with an exact-value exception list — 35% smaller snapshot, ~1.8-2x faster
    /// cold-cache queries, warm speed and pop counts within noise of `u16` (costs are
    /// always optimal). `u16`: the plain interleaved table.
    #[arg(long = "alt-format", value_enum, default_value_t = AltFormatArg::Packed)]
    alt_format: AltFormatArg,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum LandmarkStrategy {
    Legacy,
    Scc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum AltFormatArg {
    U16,
    Packed,
}

// Build a db_row JSON object for the first step of a macro-edge, depending on kind
fn fetch_db_row(conn: &Connection, kind: &str, id: i64) -> Option<serde_json::Value> {
    match kind {
        "door" => {
            if let Ok(mut st) = conn.prepare_cached(
                "SELECT direction,
                        tile_inside_x, tile_inside_y, tile_inside_plane,
                        tile_outside_x, tile_outside_y, tile_outside_plane,
                        location_open_x, location_open_y, location_open_plane,
                        location_closed_x, location_closed_y, location_closed_plane,
                        real_id_open, real_id_closed,
                        open_action,
                        cost, next_node_type, next_node_id, requirements
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
                    let cost: Option<f64> = r.get(16)?; let next_t: Option<String> = r.get(17)?; let next_id: Option<i64> = r.get(18)?; let req: Option<String> = r.get(19)?;
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
                    obj.insert("requirements".to_string(), req.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    Ok(serde_json::Value::Object(obj))
                });
                return row.ok();
            }
            None
        }
        "lodestone" => {
            if let Ok(mut st) = conn.prepare_cached(
                "SELECT lodestone, dest_x, dest_y, dest_plane, cost, next_node_type, next_node_id, requirements
                 FROM teleports_lodestone_nodes WHERE id = ?1"
            ) {
                let row: std::result::Result<serde_json::Value, _> = st.query_row([id], |r: &rusqlite::Row| {
                    let name: Option<String> = r.get(0)?;
                    let dx: Option<i64> = r.get(1)?; let dy: Option<i64> = r.get(2)?; let dp: Option<i64> = r.get(3)?;
                    let cost: Option<f64> = r.get(4)?; let next_t: Option<String> = r.get(5)?; let next_id: Option<i64> = r.get(6)?; let req: Option<String> = r.get(7)?;
                    let mut obj = serde_json::Map::new();
                    if let Some(s) = name { obj.insert("lodestone".to_string(), serde_json::Value::String(s)); }
                    obj.insert("dest".to_string(), match (dx,dy,dp) { (Some(x),Some(y),Some(p)) => serde_json::json!([x as i32,y as i32,p as i32]), _ => serde_json::Value::Null });
                    obj.insert("cost".to_string(), cost.map(|c| serde_json::Value::from(c as f32)).unwrap_or(serde_json::Value::Null));
                    obj.insert("next_node_type".to_string(), next_t.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    obj.insert("next_node_id".to_string(), next_id.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("requirements".to_string(), req.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    Ok(serde_json::Value::Object(obj))
                });
                return row.ok();
            }
            None
        }
        "object" => {
            if let Ok(mut st) = conn.prepare_cached(
                "SELECT match_type, object_id, object_name, action,
                        dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane,
                        orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane,
                        search_radius,
                        cost, next_node_type, next_node_id, requirements
                 FROM teleports_object_nodes WHERE id = ?1"
            ) {
                let row: std::result::Result<serde_json::Value, _> = st.query_row([id], |r: &rusqlite::Row| {
                    let mt: Option<String> = r.get(0)?; let oid: Option<i64> = r.get(1)?; let oname: Option<String> = r.get(2)?; let action: Option<String> = r.get(3)?;
                    let dminx: Option<i64> = r.get(4)?; let dmaxx: Option<i64> = r.get(5)?; let dminy: Option<i64> = r.get(6)?; let dmaxy: Option<i64> = r.get(7)?; let dp: Option<i64> = r.get(8)?;
                    let ominx: Option<i64> = r.get(9)?; let omaxx: Option<i64> = r.get(10)?; let ominy: Option<i64> = r.get(11)?; let omaxy: Option<i64> = r.get(12)?; let op: Option<i64> = r.get(13)?;
                    let sr: Option<i64> = r.get(14)?;
                    let cost: Option<f64> = r.get(15)?; let next_t: Option<String> = r.get(16)?; let next_id: Option<i64> = r.get(17)?; let req: Option<String> = r.get(18)?;
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
                    obj.insert("requirements".to_string(), req.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    Ok(serde_json::Value::Object(obj))
                });
                return row.ok();
            }
            None
        }
        "npc" => {
            if let Ok(mut st) = conn.prepare_cached(
                "SELECT match_type, npc_id, npc_name, action,
                        dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane,
                        orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane,
                        search_radius,
                        cost, next_node_type, next_node_id, requirements
                 FROM teleports_npc_nodes WHERE id = ?1"
            ) {
                let row: std::result::Result<serde_json::Value, _> = st.query_row([id], |r: &rusqlite::Row| {
                    let mt: Option<String> = r.get(0)?; let nid: Option<i64> = r.get(1)?; let nname: Option<String> = r.get(2)?; let action: Option<String> = r.get(3)?;
                    let dminx: Option<i64> = r.get(4)?; let dmaxx: Option<i64> = r.get(5)?; let dminy: Option<i64> = r.get(6)?; let dmaxy: Option<i64> = r.get(7)?; let dp: Option<i64> = r.get(8)?;
                    let ominx: Option<i64> = r.get(9)?; let omaxx: Option<i64> = r.get(10)?; let ominy: Option<i64> = r.get(11)?; let omaxy: Option<i64> = r.get(12)?; let op: Option<i64> = r.get(13)?;
                    let sr: Option<i64> = r.get(14)?;
                    let cost: Option<f64> = r.get(15)?; let next_t: Option<String> = r.get(16)?; let next_id: Option<i64> = r.get(17)?; let req: Option<String> = r.get(18)?;
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
                    obj.insert("requirements".to_string(), req.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    Ok(serde_json::Value::Object(obj))
                });
                return row.ok();
            }
            None
        }
        "item" => {
            if let Ok(mut st) = conn.prepare_cached(
                "SELECT match_type, name, item_id, action,
                        dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane,
                        cost, next_node_type, next_node_id, requirements
                 FROM teleports_item_nodes WHERE id = ?1"
            ) {
                let row: std::result::Result<serde_json::Value, _> = st.query_row([id], |r: &rusqlite::Row| {
                    let mt: Option<String> = r.get(0)?;
                    let name: Option<String> = r.get(1)?;
                    let iid: Option<i64> = r.get(2)?;
                    let action: Option<String> = r.get(3)?;
                    let dminx: Option<i64> = r.get(4)?; let dmaxx: Option<i64> = r.get(5)?; let dminy: Option<i64> = r.get(6)?; let dmaxy: Option<i64> = r.get(7)?; let dp: Option<i64> = r.get(8)?;
                    let cost: Option<f64> = r.get(9)?; let next_t: Option<String> = r.get(10)?; let next_id: Option<i64> = r.get(11)?; let req: Option<String> = r.get(12)?;
                    let mut obj = serde_json::Map::new();
                    obj.insert("match_type".to_string(), mt.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    obj.insert("name".to_string(), name.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
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
                    obj.insert("requirements".to_string(), req.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    Ok(serde_json::Value::Object(obj))
                });
                return row.ok();
            }
            None
        }
        "poa_item" => {
            if let Ok(mut st) = conn.prepare_cached(
                "SELECT item_id, action, action2, action3,
                        dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane,
                        cost, requirements
                 FROM teleports_POA_nodes WHERE id = ?1"
            ) {
                let row: std::result::Result<serde_json::Value, _> = st.query_row([id], |r: &rusqlite::Row| {
                    let iid: Option<i64> = r.get(0)?;
                    let action: Option<String> = r.get(1)?;
                    let action2: Option<String> = r.get(2)?;
                    let action3: Option<String> = r.get(3)?;
                    let dminx: Option<i64> = r.get(4)?; let dmaxx: Option<i64> = r.get(5)?; let dminy: Option<i64> = r.get(6)?; let dmaxy: Option<i64> = r.get(7)?; let dp: Option<i64> = r.get(8)?;
                    let cost: Option<f64> = r.get(9)?; let req: Option<String> = r.get(10)?;
                    let mut obj = serde_json::Map::new();
                    obj.insert("item_id".to_string(), iid.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("action".to_string(), action.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    obj.insert("action2".to_string(), action2.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    obj.insert("action3".to_string(), action3.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    obj.insert("dest_min_x".to_string(), dminx.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("dest_max_x".to_string(), dmaxx.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("dest_min_y".to_string(), dminy.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("dest_max_y".to_string(), dmaxy.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("dest_plane".to_string(), dp.map(|v| serde_json::Value::from(v as i32)).unwrap_or(serde_json::Value::Null));
                    obj.insert("cost".to_string(), cost.map(|c| serde_json::Value::from(c as f32)).unwrap_or(serde_json::Value::Null));
                    obj.insert("requirements".to_string(), req.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    Ok(serde_json::Value::Object(obj))
                });
                return row.ok();
            }
            None
        }
        "use_on" => {
            if let Ok(mut st) = conn.prepare_cached(
                "SELECT item_id, object_id,
                        dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane,
                        orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane,
                        cost, next_node_type, next_node_id, requirements
                 FROM teleports_useOn_nodes WHERE id = ?1"
            ) {
                let row: std::result::Result<serde_json::Value, _> = st.query_row([id], |r: &rusqlite::Row| {
                    let iid: Option<i64> = r.get(0)?; let oid: Option<i64> = r.get(1)?;
                    let dminx: Option<i64> = r.get(2)?; let dmaxx: Option<i64> = r.get(3)?; let dminy: Option<i64> = r.get(4)?; let dmaxy: Option<i64> = r.get(5)?; let dp: Option<i64> = r.get(6)?;
                    let ominx: Option<i64> = r.get(7)?; let omaxx: Option<i64> = r.get(8)?; let ominy: Option<i64> = r.get(9)?; let omaxy: Option<i64> = r.get(10)?; let op: Option<i64> = r.get(11)?;
                    let cost: Option<f64> = r.get(12)?; let next_t: Option<String> = r.get(13)?; let next_id: Option<i64> = r.get(14)?; let req: Option<String> = r.get(15)?;
                    let mut obj = serde_json::Map::new();
                    obj.insert("item_id".to_string(), iid.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("object_id".to_string(), oid.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
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
                    obj.insert("cost".to_string(), cost.map(|c| serde_json::Value::from(c as f32)).unwrap_or(serde_json::Value::Null));
                    obj.insert("next_node_type".to_string(), next_t.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    obj.insert("next_node_id".to_string(), next_id.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));
                    obj.insert("requirements".to_string(), req.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
                    Ok(serde_json::Value::Object(obj))
                });
                return row.ok();
            }
            None
        }
        "ifslot" => {
            if let Ok(mut st) = conn.prepare_cached(
                "SELECT interface_id, component_id, slot_id, click_id,
                        dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane,
                        cost, next_node_type, next_node_id, requirements
                 FROM teleports_ifslot_nodes WHERE id = ?1"
            ) {
                let row: std::result::Result<serde_json::Value, _> = st.query_row([id], |r: &rusqlite::Row| {
                    let iface: Option<i64> = r.get(0)?; let comp: Option<i64> = r.get(1)?; let slot: Option<i64> = r.get(2)?; let click: Option<i64> = r.get(3)?;
                    let dminx: Option<i64> = r.get(4)?; let dmaxx: Option<i64> = r.get(5)?; let dminy: Option<i64> = r.get(6)?; let dmaxy: Option<i64> = r.get(7)?; let dp: Option<i64> = r.get(8)?;
                    let cost: Option<f64> = r.get(9)?; let next_t: Option<String> = r.get(10)?; let next_id: Option<i64> = r.get(11)?; let req: Option<String> = r.get(12)?;
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
                    obj.insert("requirements".to_string(), req.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
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
    // Best-effort pragmas; ignore failures. mmap_size/cache_size/temp_store are the ones
    // that matter for large sequential scans on a read-only connection.
    let _ = conn.execute_batch(
        r#"
        PRAGMA query_only=ON;
        PRAGMA foreign_keys=OFF;
        PRAGMA journal_mode=OFF;
        PRAGMA synchronous=OFF;
        PRAGMA mmap_size=1073741824;
        PRAGMA cache_size=-262144;
        PRAGMA temp_store=MEMORY;
        "#,
    );
    Ok(conn)
}

fn main() -> Result<()> {
    let subscriber = FmtSubscriber::builder().with_ansi(false).json().finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    let args = Args::parse();
    info!(?args, "starting builder");
    let t_start = std::time::Instant::now();
    let stage = |name: &str, t: std::time::Instant| {
        info!(stage = name, elapsed_ms = t.elapsed().as_millis() as u64, total_ms = t_start.elapsed().as_millis() as u64, "stage done");
    };

    let landmark_count = if args.landmarks % build::landmarks::LANDMARK_ALIGN != 0 {
        let aligned = align_landmark_count(args.landmarks);
        tracing::warn!(requested = args.landmarks, aligned, "landmark count rounded up to a multiple of 16 (SIMD row stride)");
        aligned
    } else {
        args.landmarks
    };

    let conn = open_read_only(&args.sqlite_path)
        .with_context(|| format!("failed to open {:?}", args.sqlite_path))?;

    // Load tiles
    let t = std::time::Instant::now();
    let tiles = load_all_tiles(&conn)?;
    if tiles.is_empty() {
        anyhow::bail!("no tiles found in DB");
    }

    // Node ids are sequential 0..n-1 in tile sort order; the packed keys are therefore
    // ascending, which is what the snapshot's binary-search coordinate lookup relies on.
    let node_count = tiles.len();
    let coords_packed: Vec<u32> = tiles
        .iter()
        .map(|t| {
            assert!(
                (0..32768).contains(&t.x) && (0..32768).contains(&t.y) && (0..4).contains(&t.plane),
                "tile coordinate out of packed range: ({}, {}, {})", t.x, t.y, t.plane
            );
            pack_coord(t.x, t.y, t.plane)
        })
        .collect();
    // Hard bail, not debug_assert: duplicate/overlapping tiles (e.g. malformed
    // tiles_regions rows) would silently corrupt the snapshot's binary-search
    // coordinate lookup in release builds, resolving requests to wrong nodes.
    if let Some(w) = coords_packed.windows(2).find(|w| w[0] >= w[1]) {
        anyhow::bail!(
            "packed tile coords are not strictly ascending (…{}, {}…): duplicate or \
             overlapping tiles in the DB (malformed tiles_regions rows?); refusing to \
             build a snapshot whose coordinate lookup would be corrupt",
            w[0], w[1]
        );
    }

    stage("load_tiles", t);

    // Coordinate -> node id resolution: binary search over the packed keys (identical
    // results to the old HashMap, minus its ~36 MB and per-probe hashing).
    let node_id_of = build::graph::NodeIndex::new(&coords_packed);

    // Walk graph, emitted straight into the snapshot's CSR (+ diagonal bitmap) form.
    let t = std::time::Instant::now();
    let walk = compile_walk_csr(&tiles, &coords_packed);
    let walk_offsets = &walk.offsets;
    let walk_csr_dst = &walk.dst;

    // Fail fast (before the metadata and ALT stages): both structural invariants the
    // query engine relies on are checked on the CSR as soon as it exists.
    //
    // Bidirectional search reuses the forward walk CSR as its reverse graph (and so does
    // the ALT stage below), which is only sound if every walk edge has its mirror (the
    // mirror's weight is then equal: walk weights are implied by the direction class,
    // and the reverse of a cardinal/diagonal step is a cardinal/diagonal step). The
    // current rules guarantee it for cardinals and it holds empirically for diagonals;
    // assert so future map data can't silently break search correctness.
    {
        use rayon::prelude::*;
        // Read-only CSR scan; sum-reduce the asymmetry count across nodes (roadmap 7.5).
        let asym: usize = (0..node_count)
            .into_par_iter()
            .map(|u| {
                let (s, e) = (walk_offsets[u] as usize, walk_offsets[u + 1] as usize);
                let mut bad = 0usize;
                'edge: for slot in s..e {
                    let v = walk_csr_dst[slot] as usize;
                    let (vs, ve) = (walk_offsets[v] as usize, walk_offsets[v + 1] as usize);
                    for vslot in vs..ve {
                        if walk_csr_dst[vslot] as usize == u {
                            continue 'edge;
                        }
                    }
                    bad += 1;
                }
                bad
            })
            .sum();
        if asym > 0 {
            anyhow::bail!("walk graph is not symmetric: {asym} edges lack a mirror; bidirectional search would be unsound");
        }
    }

    // Canonical pruning (Phase E) resolves direction -> CSR slot via
    // popcount(mask & ((1<<d)-1)), which requires every CSR row to be emitted in
    // ascending direction-bit order. The emission loop guarantees it today; pin it so
    // future edge-rule changes can't silently break query-time slot addressing.
    {
        use rayon::prelude::*;
        use navpath_core::snapshot::unpack_coord;
        const DIR_DELTAS: [(i32, i32); 8] =
            [(-1, 0), (0, -1), (1, 0), (0, 1), (-1, 1), (-1, -1), (1, -1), (1, 1)];
        let bad: usize = (0..node_count)
            .into_par_iter()
            .map(|u| {
                let (ux, uy, up) = unpack_coord(coords_packed[u]);
                let (s, e) = (walk_offsets[u] as usize, walk_offsets[u + 1] as usize);
                let mut prev: i32 = -1;
                for &v in &walk_csr_dst[s..e] {
                    let (vx, vy, vp) = unpack_coord(coords_packed[v as usize]);
                    let d = DIR_DELTAS
                        .iter()
                        .position(|&(dx, dy)| vp == up && vx - ux == dx && vy - uy == dy);
                    match d {
                        Some(d) if (d as i32) > prev => prev = d as i32,
                        _ => return 1usize,
                    }
                }
                0
            })
            .sum();
        if bad > 0 {
            anyhow::bail!(
                "{bad} CSR rows violate ascending direction-bit order (or contain \
                 non-adjacent edges); canonical slot addressing would be unsound"
            );
        }
    }

    // Walk components (the snapshot's reachability-precheck section). Labels are in
    // order of each component's lowest node id, as before; the section stores u16 ids,
    // so refuse to silently wrap them.
    let (walk_comp, walk_comp_count) = walk_components(walk_offsets, walk_csr_dst);
    if walk_comp_count > u16::MAX as usize + 1 {
        anyhow::bail!(
            "{walk_comp_count} walk components exceed the u16 component-id range of the \
             snapshot's comp section (max 65536)"
        );
    }
    let comp_ids: Vec<u16> = walk_comp.iter().map(|&c| c as u16).collect();
    let walk_components = walk_comp_count as u32;
    info!(walk_components, walk_edges = walk.edges(), "compiled walk CSR and components");
    stage("walk_graph", t);

    // Flatten chains into macro-edges with cycle detection and deterministic ordering
    let t = std::time::Instant::now();
    let (metas, gmetas) = flatten_all_chains(&conn, &node_id_of)?;
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
                "poa_item" => 7u32,
                "use_on" => 8u32,
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

        // The first step's db_row (with its next_db_row already attached) was just built
        // for steps_json above; reuse it for the meta-level entry instead of re-querying.
        let first_db_row = steps_json.first().and_then(|s| s.get("db_row")).cloned();

        // Start building meta object
        let mut meta_obj = serde_json::Map::new();
        meta_obj.insert("kind".to_string(), serde_json::Value::String(match k { 1=>"door",2=>"lodestone",3=>"npc",4=>"object",5=>"item",6=>"ifslot",7=>"poa_item",8=>"use_on", _=>"unknown" }.to_string()));
        meta_obj.insert("first_id".to_string(), serde_json::Value::from(id));
        meta_obj.insert("steps".to_string(), serde_json::Value::from(steps_json));
        meta_obj.insert("requirements".to_string(), serde_json::Value::from(m.requirement_ids.clone()));

        if let Some(v) = first_db_row {
            meta_obj.insert("db_row".to_string(), v);
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

    // Load Fairy Rings (needed before the ALT stage: the landmark tables must cover
    // every edge the search can relax mid-query, and fairy hops are such edges).
    let fairy_ring_rows = load_fairy_rings(&conn, &node_id_of).unwrap_or_else(|e| {
        // Fail-soft: log warning and proceed with empty list if table doesn't exist
        tracing::warn!(error = ?e, "failed to load fairy rings (table may not exist); proceeding with empty list");
        Vec::new()
    });
    info!(count = fairy_ring_rows.len(), "loaded fairy rings from SQLite");

    // ALT graph edges: walk + macro + the full fairy-ring clique. Two admissibility
    // details (the runtime graph must be a SUBSET of this graph, edge-for-edge, with
    // weights >= the ones used here):
    //  - fairy hops connect every ring to every other ring at the destination ring's
    //    cost; per-request eligibility only removes rings, which keeps bounds valid;
    //  - quick-tele can lower lodestone-first macro edges (kind_first == 2) to 2400ms
    //    at query time, so the table graph uses min(w, 2400) for those.
    let mut alt_macro_src = macro_src.clone();
    let mut alt_macro_dst = macro_dst.clone();
    let mut alt_macro_w: Vec<f32> = macro_w
        .iter()
        .zip(macro_kind_first.iter())
        .map(|(&w, &k)| if k == 2 { w.min(2400.0) } else { w })
        .collect();
    for a in &fairy_ring_rows {
        for b in &fairy_ring_rows {
            if a.node_id != b.node_id {
                alt_macro_src.push(a.node_id);
                alt_macro_dst.push(b.node_id);
                alt_macro_w.push(b.cost);
            }
        }
    }

    stage("metadata", t);

    // Landmarks + ALT tables over the table graph: the walk CSR (symmetric, so it serves
    // both directions) plus the macro/fairy edges above.
    let t = std::time::Instant::now();
    let alt_graph = AltGraph::new(
        node_count,
        walk_offsets,
        walk_csr_dst,
        &walk.diag,
        &alt_macro_src,
        &alt_macro_dst,
        &alt_macro_w,
    );
    let structure = Structure::new(walk_comp, walk_comp_count, alt_graph.fwd.pairs());
    {
        let top: Vec<(usize, u32)> = structure.scc_size.iter().take(8).enumerate().map(|(i, &s)| (s, structure.scc_wcc[i])).collect();
        info!(
            sccs = structure.scc_size.len(),
            wccs = structure.wcc_size.len(),
            main_scc = structure.scc_size[0],
            main_wcc = structure.wcc_size[structure.scc_wcc[0] as usize],
            top_sccs_size_wcc = ?top,
            top_wccs = ?structure.wcc_size.iter().take(8).collect::<Vec<_>>(),
            "table-graph structure"
        );
    }
    let alt_cfg = AltConfig {
        count: landmark_count as usize,
        strategy: match args.landmark_strategy {
            LandmarkStrategy::Legacy => Strategy::Legacy,
            LandmarkStrategy::Scc => Strategy::Scc,
        },
        local_fill: args.local_fill,
    };
    let (plan, lm_tab) = build_alt(&alt_graph, &structure, &alt_cfg);
    let landmarks = plan.landmarks;
    if !landmarks.is_empty() {
        let lstats = table_stats(&structure, &lm_tab, landmarks.len());
        let per_scc: std::collections::BTreeMap<u32, usize> =
            plan.landmark_scc.iter().fold(Default::default(), |mut m, &s| {
                *m.entry(s).or_insert(0) += 1;
                m
            });
        info!(
            count = landmarks.len(),
            strategy = ?alt_cfg.strategy,
            local_fill_wccs = plan.fills.len(),
            columns_per_scc_rank = ?per_scc,
            usable_main_scc = lstats.usable_main,
            h0_nodes = lstats.h0_nodes,
            h0_main_wcc = lstats.h0_main_wcc,
            h0_other_wcc = lstats.h0_other_wcc,
            mean_usable = lstats.mean_usable,
            mean_usable_main = lstats.mean_usable_main,
            saturated_entries = lstats.saturated_entries,
            elapsed_ms = t.elapsed().as_millis() as u64,
            "selected landmarks and computed ALT tables"
        );
    }
    drop(alt_graph);
    stage("alt", t);

    // Encode Fairy Rings into snapshot sections
    let mut fairy_nodes: Vec<u32> = Vec::with_capacity(fairy_ring_rows.len());
    let mut fairy_cost_ms: Vec<f32> = Vec::with_capacity(fairy_ring_rows.len());
    let mut fairy_meta_offs: Vec<u32> = Vec::with_capacity(fairy_ring_rows.len());
    let mut fairy_meta_lens: Vec<u32> = Vec::with_capacity(fairy_ring_rows.len());
    let mut fairy_meta_blob: Vec<u8> = Vec::new();

    for ring in &fairy_ring_rows {
        fairy_nodes.push(ring.node_id);
        fairy_cost_ms.push(ring.cost);

        // Build per-ring JSON metadata
        let mut meta_obj = serde_json::Map::new();
        meta_obj.insert("object_id".to_string(), serde_json::Value::from(ring.object_id));
        meta_obj.insert("x".to_string(), serde_json::Value::from(ring.x));
        meta_obj.insert("y".to_string(), serde_json::Value::from(ring.y));
        meta_obj.insert("plane".to_string(), serde_json::Value::from(ring.plane));
        meta_obj.insert("code".to_string(), serde_json::Value::String(ring.code.clone()));
        meta_obj.insert("action".to_string(), ring.action.clone().map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
        meta_obj.insert("requirements".to_string(), serde_json::Value::from(ring.requirements.clone()));
        meta_obj.insert("next_node_type".to_string(), ring.next_node_type.clone().map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
        meta_obj.insert("next_node_id".to_string(), ring.next_node_id.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null));

        let meta = serde_json::Value::Object(meta_obj);
        let bytes = serde_json::to_vec(&meta).unwrap_or_else(|_| b"{}".to_vec());
        let off = fairy_meta_blob.len() as u32;
        fairy_meta_offs.push(off);
        fairy_meta_lens.push(bytes.len() as u32);
        fairy_meta_blob.extend_from_slice(&bytes);
    }

    let t = std::time::Instant::now();
    let write_opts = WriteOptions {
        alt_format: match args.alt_format {
            AltFormatArg::U16 => AltFormat::U16,
            AltFormatArg::Packed => AltFormat::Packed,
        },
    };
    let res = write_snapshot(
        &args.out_snapshot,
        &SnapshotSections {
            coords_packed: &coords_packed,
            walk_offsets,
            walk_dst: walk_csr_dst,
            walk_diag: &walk.diag,
            comp: &comp_ids,
            walk_components,
            macro_src: &macro_src,
            macro_dst: &macro_dst,
            macro_w: &macro_w,
            macro_kind_first: &macro_kind_first,
            macro_id_first: &macro_id_first,
            macro_meta_offs: &macro_meta_offs,
            macro_meta_lens: &macro_meta_lens,
            macro_meta_blob: &macro_meta_blob,
            req_tags: &req_tags,
            landmarks: &landmarks,
            lm_tab: &lm_tab,
            fairy_nodes: &fairy_nodes,
            fairy_cost_ms: &fairy_cost_ms,
            fairy_meta_offs: &fairy_meta_offs,
            fairy_meta_lens: &fairy_meta_lens,
            fairy_meta_blob: &fairy_meta_blob,
        },
        &write_opts,
    );

    match res {
        Ok(info) => {
            info!(manifest = ?info.manifest, hash = ?info.hash, "wrote snapshot");
            stage("write", t);
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
        let bytes: Vec<u8> = tiles.iter().map(|t| (t.walk_mask & 0xFF) as u8).collect();
        f.write_all(&bytes)?;
        f.flush()?;
        info!(path = ?out_tiles, bytes = tiles.len(), "wrote tiles.bin");
    }

    if !args.no_walkable {
        let out_walkable = args
            .out_walkable
            .unwrap_or_else(|| args.out_snapshot.with_file_name("walkableTiles.bin"));
        let bytes = encode_walkable_tiles(&tiles);
        let mut f = File::create(&out_walkable)
            .with_context(|| format!("creating {:?}", out_walkable))?;
        f.write_all(&bytes)?;
        f.flush()?;
        info!(path = ?out_walkable, bytes = bytes.len(), tiles = tiles.len(), "wrote walkableTiles.bin");
    }

    Ok(())
}

/// walkableTiles.bin, version 1 (all integers little-endian):
///
/// ```text
/// magic   "WTIL"                    4 bytes
/// version u8 = 1
/// chunks  u32                       number of 64x64 chunks that follow
/// chunk*  plane u8, rx u16, ry u16  region coords (rx = x / 64, ry = y / 64)
///         bitmap [u8; 512]          bit i = (y % 64) * 64 + (x % 64), LSB first;
///                                   set iff the tile is in the DB (walkable)
/// ```
///
/// Chunks are sorted by (plane, ry, rx) and only non-empty ones are written, so the file
/// is ~517 bytes per populated region (~400 KB for the whole world). Presence is the
/// `/tile/exists` set: every DB row, including the handful of teleport-only tiles whose
/// walk_mask is 0. Consumed by Hoor2's `WalkMap` (Area.getRandomWalkableTile()).
fn encode_walkable_tiles(tiles: &[build::load_sqlite::Tile]) -> Vec<u8> {
    use std::collections::BTreeMap;
    let mut chunks: BTreeMap<(u8, u16, u16), [u8; 512]> = BTreeMap::new();
    for t in tiles {
        let key = (t.plane as u8, (t.y / 64) as u16, (t.x / 64) as u16);
        let bm = chunks.entry(key).or_insert([0u8; 512]);
        let i = ((t.y % 64) * 64 + (t.x % 64)) as usize;
        bm[i / 8] |= 1 << (i % 8);
    }
    let mut out = Vec::with_capacity(9 + chunks.len() * 517);
    out.extend_from_slice(b"WTIL");
    out.push(1);
    out.extend_from_slice(&(chunks.len() as u32).to_le_bytes());
    for ((plane, ry, rx), bm) in &chunks {
        out.push(*plane);
        out.extend_from_slice(&rx.to_le_bytes());
        out.extend_from_slice(&ry.to_le_bytes());
        out.extend_from_slice(bm);
    }
    out
}
