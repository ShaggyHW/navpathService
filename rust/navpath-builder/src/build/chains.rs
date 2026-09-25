use std::collections::HashSet;

use super::graph::NodeIndex;

use anyhow::Result;
use rusqlite::{params, Connection, Row, OptionalExtension};

use super::load_sqlite::Tile;

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

#[cfg(test)]
mod tests {
    use super::parse_requirements;

    #[test]
    fn parses_empty_and_none() {
        assert_eq!(parse_requirements(None), Vec::<i64>::new());
        assert_eq!(parse_requirements(Some("".to_string())), Vec::<i64>::new());
        assert_eq!(parse_requirements(Some("  ".to_string())), Vec::<i64>::new());
    }

    #[test]
    fn parses_single_requirement() {
        assert_eq!(parse_requirements(Some("78".to_string())), vec![78]);
        assert_eq!(parse_requirements(Some(" 78 ".to_string())), vec![78]);
    }

    #[test]
    fn parses_multi_requirement_semicolon() {
        assert_eq!(parse_requirements(Some("55;58".to_string())), vec![55, 58]);
        assert_eq!(parse_requirements(Some("55; 58".to_string())), vec![55, 58]);
        assert_eq!(parse_requirements(Some("55;;58".to_string())), vec![55, 58]);
        assert_eq!(parse_requirements(Some("55;58;".to_string())), vec![55, 58]);
    }

    #[test]
    fn parses_multi_requirement_comma() {
        assert_eq!(parse_requirements(Some("55,58".to_string())), vec![55, 58]);
        assert_eq!(parse_requirements(Some("55, 58".to_string())), vec![55, 58]);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    Door,
    Lodestone,
    Npc,
    Object,
    Item,
    Ifslot,
    Poa,
    UseOn,
}

impl NodeKind {
    fn as_str(&self) -> &'static str {
        match self {
            NodeKind::Door => "door",
            NodeKind::Lodestone => "lodestone",
            NodeKind::Npc => "npc",
            NodeKind::Object => "object",
            NodeKind::Item => "item",
            NodeKind::Ifslot => "ifslot",
            NodeKind::Poa => "poa_item",
            NodeKind::UseOn => "use_on",
        }
    }
    fn parse(s: &str) -> Option<NodeKind> {
        // Case/spelling tolerant: the useOn table is named `teleports_useOn_nodes`, so
        // producers may write "useOn" or "use_on" in a next_node_type column.
        match s.trim().to_ascii_lowercase().as_str() {
            "door" => Some(NodeKind::Door),
            "lodestone" => Some(NodeKind::Lodestone),
            "npc" => Some(NodeKind::Npc),
            "object" => Some(NodeKind::Object),
            "item" => Some(NodeKind::Item),
            "ifslot" => Some(NodeKind::Ifslot),
            "poa_item" => Some(NodeKind::Poa),
            "use_on" | "useon" => Some(NodeKind::UseOn),
            _ => None,
        }
    }
}

/// True if `name` exists as a table in this DB. The useOn table postdates older
/// snapshots' DBs, so its queries are skipped rather than failing the whole build.
fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND lower(name) = lower(?1)",
        params![name],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

#[derive(Debug, Clone)]
pub struct ChainStepMeta {
    pub kind: &'static str,
    pub id: i64,
    pub cost: f32,
    pub requirements: Vec<i64>,
    pub lodestone: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MacroEdgeMeta {
    pub src: u32,
    pub dst: u32,
    pub cost: f32,
    pub requirement_ids: Vec<i64>,
    pub steps: Vec<ChainStepMeta>,
}

struct StepRow {
    dest: Option<(i32, i32, i32)>,
    next_kind: Option<NodeKind>,
    next_id: Option<i64>,
    cost: f32,
    requirements: Vec<i64>,
    lodestone: Option<String>,
}

fn fetch_step(conn: &Connection, kind: NodeKind, id: i64) -> Result<Option<StepRow>> {
    match kind {
        NodeKind::Door => {
            let mut st = conn.prepare_cached(
                "SELECT tile_inside_x, tile_inside_y, tile_inside_plane, next_node_type, next_node_id, cost, requirements FROM teleports_door_nodes WHERE id = ?1",
            )?;
            let row = st.query_row(params![id], |r: &Row| {
                let dx: Option<i64> = r.get(0)?;
                let dy: Option<i64> = r.get(1)?;
                let dp: Option<i64> = r.get(2)?;
                let ntype: Option<String> = r.get(3)?;
                let nid: Option<i64> = r.get(4)?;
                let cost: f64 = r.get(5)?;
                let req: Option<String> = r.get(6)?;
                Ok(StepRow {
                    dest: match (dx, dy, dp) {
                        (Some(x), Some(y), Some(p)) => Some((x as i32, y as i32, p as i32)),
                        _ => None,
                    },
                    next_kind: ntype.and_then(|s| NodeKind::parse(&s)),
                    next_id: nid,
                    cost: if cost.is_finite() && cost >= 0.0 { cost as f32 } else { 0.0 },
                    requirements: parse_requirements(req),
                    lodestone: None,
                })
            }).optional()?;
            Ok(row)
        }
        NodeKind::Lodestone => {
            // Production DBs carry the lodestone's name column: fetch it in the same
            // query (it used to be a second per-step query). Minimal/older schemas
            // without the column fail this prepare and take the name-less path below,
            // exactly as the old best-effort second query did. A name that can't be read
            // as text stays None, also as before.
            if let Ok(mut st) = conn.prepare_cached(
                "SELECT dest_x, dest_y, dest_plane, next_node_type, next_node_id, cost, requirements, lodestone FROM teleports_lodestone_nodes WHERE id = ?1",
            ) {
                let row = st.query_row(params![id], |r: &Row| {
                    let dx: Option<i64> = r.get(0)?;
                    let dy: Option<i64> = r.get(1)?;
                    let dp: Option<i64> = r.get(2)?;
                    let ntype: Option<String> = r.get(3)?;
                    let nid: Option<i64> = r.get(4)?;
                    let cost: f64 = r.get(5)?;
                    let req: Option<String> = r.get(6)?;
                    Ok(StepRow {
                        dest: match (dx, dy, dp) {
                            (Some(x), Some(y), Some(p)) => Some((x as i32, y as i32, p as i32)),
                            _ => None,
                        },
                        next_kind: ntype.and_then(|s| NodeKind::parse(&s)),
                        next_id: nid,
                        cost: if cost.is_finite() && cost >= 0.0 { cost as f32 } else { 0.0 },
                        requirements: parse_requirements(req),
                        lodestone: r.get::<_, Option<String>>(7).ok().flatten(),
                    })
                }).optional()?;
                return Ok(row);
            }
            let mut st = conn.prepare_cached(
                "SELECT dest_x, dest_y, dest_plane, next_node_type, next_node_id, cost, requirements FROM teleports_lodestone_nodes WHERE id = ?1",
            )?;
            let row = st.query_row(params![id], |r: &Row| {
                let dx: Option<i64> = r.get(0)?;
                let dy: Option<i64> = r.get(1)?;
                let dp: Option<i64> = r.get(2)?;
                let ntype: Option<String> = r.get(3)?;
                let nid: Option<i64> = r.get(4)?;
                let cost: f64 = r.get(5)?;
                let req: Option<String> = r.get(6)?;
                Ok(StepRow {
                    dest: match (dx, dy, dp) {
                        (Some(x), Some(y), Some(p)) => Some((x as i32, y as i32, p as i32)),
                        _ => None,
                    },
                    next_kind: ntype.and_then(|s| NodeKind::parse(&s)),
                    next_id: nid,
                    cost: if cost.is_finite() && cost >= 0.0 { cost as f32 } else { 0.0 },
                    requirements: parse_requirements(req),
                    lodestone: None,
                })
            }).optional()?;
            Ok(row)
        }
        NodeKind::Npc => {
            let mut st = conn.prepare_cached(
                "SELECT dest_min_x, dest_min_y, dest_plane, next_node_type, next_node_id, cost, requirements FROM teleports_npc_nodes WHERE id = ?1",
            )?;
            let row = st.query_row(params![id], |r: &Row| {
                let dx: Option<i64> = r.get(0)?;
                let dy: Option<i64> = r.get(1)?;
                let dp: Option<i64> = r.get(2)?;
                let ntype: Option<String> = r.get(3)?;
                let nid: Option<i64> = r.get(4)?;
                let cost: f64 = r.get(5)?;
                let req: Option<String> = r.get(6)?;
                Ok(StepRow {
                    dest: match (dx, dy, dp) {
                        (Some(x), Some(y), Some(p)) => Some((x as i32, y as i32, p as i32)),
                        _ => None,
                    },
                    next_kind: ntype.and_then(|s| NodeKind::parse(&s)),
                    next_id: nid,
                    cost: if cost.is_finite() && cost >= 0.0 { cost as f32 } else { 0.0 },
                    requirements: parse_requirements(req),
                    lodestone: None,
                })
            }).optional()?;
            Ok(row)
        }
        NodeKind::Object => {
            let mut st = conn.prepare_cached(
                "SELECT dest_min_x, dest_min_y, dest_plane, next_node_type, next_node_id, cost, requirements FROM teleports_object_nodes WHERE id = ?1",
            )?;
            let row = st.query_row(params![id], |r: &Row| {
                let dx: Option<i64> = r.get(0)?;
                let dy: Option<i64> = r.get(1)?;
                let dp: Option<i64> = r.get(2)?;
                let ntype: Option<String> = r.get(3)?;
                let nid: Option<i64> = r.get(4)?;
                let cost: f64 = r.get(5)?;
                let req: Option<String> = r.get(6)?;
                Ok(StepRow {
                    dest: match (dx, dy, dp) {
                        (Some(x), Some(y), Some(p)) => Some((x as i32, y as i32, p as i32)),
                        _ => None,
                    },
                    next_kind: ntype.and_then(|s| NodeKind::parse(&s)),
                    next_id: nid,
                    cost: if cost.is_finite() && cost >= 0.0 { cost as f32 } else { 0.0 },
                    requirements: parse_requirements(req),
                    lodestone: None,
                })
            }).optional()?;
            Ok(row)
        }
        NodeKind::Item => {
            let mut st = conn.prepare_cached(
                "SELECT dest_min_x, dest_min_y, dest_plane, next_node_type, next_node_id, cost, requirements FROM teleports_item_nodes WHERE id = ?1",
            )?;
            let row = st.query_row(params![id], |r: &Row| {
                let dx: Option<i64> = r.get(0)?;
                let dy: Option<i64> = r.get(1)?;
                let dp: Option<i64> = r.get(2)?;
                let ntype: Option<String> = r.get(3)?;
                let nid: Option<i64> = r.get(4)?;
                let cost: f64 = r.get(5)?;
                let req: Option<String> = r.get(6)?;
                Ok(StepRow {
                    dest: match (dx, dy, dp) {
                        (Some(x), Some(y), Some(p)) => Some((x as i32, y as i32, p as i32)),
                        _ => None,
                    },
                    next_kind: ntype.and_then(|s| NodeKind::parse(&s)),
                    next_id: nid,
                    cost: if cost.is_finite() && cost >= 0.0 { cost as f32 } else { 0.0 },
                    requirements: parse_requirements(req),
                    lodestone: None,
                })
            }).optional()?;
            Ok(row)
        }
        NodeKind::Poa => {
            // POA nodes are standalone global teleports (like items): no next_node chaining.
            let mut st = conn.prepare_cached(
                "SELECT dest_min_x, dest_min_y, dest_plane, cost, requirements FROM teleports_POA_nodes WHERE id = ?1",
            )?;
            let row = st.query_row(params![id], |r: &Row| {
                let dx: Option<i64> = r.get(0)?;
                let dy: Option<i64> = r.get(1)?;
                let dp: Option<i64> = r.get(2)?;
                let cost: f64 = r.get(3)?;
                let req: Option<String> = r.get(4)?;
                Ok(StepRow {
                    dest: match (dx, dy, dp) {
                        (Some(x), Some(y), Some(p)) => Some((x as i32, y as i32, p as i32)),
                        _ => None,
                    },
                    next_kind: None,
                    next_id: None,
                    cost: if cost.is_finite() && cost >= 0.0 { cost as f32 } else { 0.0 },
                    requirements: parse_requirements(req),
                    lodestone: None,
                })
            }).optional()?;
            Ok(row)
        }
        NodeKind::UseOn => {
            // "Use item X on object Y" at a fixed spot: positional source (orig_*) like
            // objects/npcs, so it flattens into a macro edge, and it can chain onwards.
            let mut st = conn.prepare_cached(
                "SELECT dest_min_x, dest_min_y, dest_plane, next_node_type, next_node_id, cost, requirements FROM teleports_useOn_nodes WHERE id = ?1",
            )?;
            let row = st.query_row(params![id], |r: &Row| {
                let dx: Option<i64> = r.get(0)?;
                let dy: Option<i64> = r.get(1)?;
                let dp: Option<i64> = r.get(2)?;
                let ntype: Option<String> = r.get(3)?;
                let nid: Option<i64> = r.get(4)?;
                let cost: f64 = r.get::<_, Option<f64>>(5)?.unwrap_or(0.0);
                let req: Option<String> = r.get(6)?;
                Ok(StepRow {
                    dest: match (dx, dy, dp) {
                        (Some(x), Some(y), Some(p)) => Some((x as i32, y as i32, p as i32)),
                        _ => None,
                    },
                    next_kind: ntype.and_then(|s| NodeKind::parse(&s)),
                    next_id: nid,
                    cost: if cost.is_finite() && cost >= 0.0 { cost as f32 } else { 0.0 },
                    requirements: parse_requirements(req),
                    lodestone: None,
                })
            }).optional()?;
            Ok(row)
        }
        NodeKind::Ifslot => {
            let mut st = conn.prepare_cached(
                "SELECT dest_min_x, dest_min_y, dest_plane, next_node_type, next_node_id, cost, requirements FROM teleports_ifslot_nodes WHERE id = ?1",
            )?;
            let row = st.query_row(params![id], |r: &Row| {
                let dx: Option<i64> = r.get(0)?;
                let dy: Option<i64> = r.get(1)?;
                let dp: Option<i64> = r.get(2)?;
                let ntype: Option<String> = r.get(3)?;
                let nid: Option<i64> = r.get(4)?;
                let cost: f64 = r.get(5)?;
                let req: Option<String> = r.get(6)?;
                Ok(StepRow {
                    dest: match (dx, dy, dp) {
                        (Some(x), Some(y), Some(p)) => Some((x as i32, y as i32, p as i32)),
                        _ => None,
                    },
                    next_kind: ntype.and_then(|s| NodeKind::parse(&s)),
                    next_id: nid,
                    cost: if cost.is_finite() && cost >= 0.0 { cost as f32 } else { 0.0 },
                    requirements: parse_requirements(req),
                    lodestone: None,
                })
            }).optional()?;
            Ok(row)
        }
    }
}

fn collect_incoming_pairs(conn: &Connection) -> Result<HashSet<(NodeKind, i64)>> {
    let mut set: HashSet<(NodeKind, i64)> = HashSet::new();
    // Helper to scan a table's next_node_type/id
    let mut add_from = |sql: &str| -> Result<()> {
        let mut st = conn.prepare_cached(sql)?;
        let rows = st.query_map([], |r: &Row| {
            let nt: Option<String> = r.get(0)?;
            let nid: Option<i64> = r.get(1)?;
            Ok((nt, nid))
        })?;
        for r in rows {
            let (nt, nid) = r?;
            if let (Some(nt), Some(nid)) = (nt, nid) {
                if let Some(k) = NodeKind::parse(&nt) { set.insert((k, nid)); }
            }
        }
        Ok(())
    };
    add_from("SELECT next_node_type, next_node_id FROM teleports_door_nodes WHERE next_node_type IS NOT NULL AND next_node_id IS NOT NULL")?;
    add_from("SELECT next_node_type, next_node_id FROM teleports_lodestone_nodes WHERE next_node_type IS NOT NULL AND next_node_id IS NOT NULL")?;
    add_from("SELECT next_node_type, next_node_id FROM teleports_npc_nodes WHERE next_node_type IS NOT NULL AND next_node_id IS NOT NULL")?;
    add_from("SELECT next_node_type, next_node_id FROM teleports_object_nodes WHERE next_node_type IS NOT NULL AND next_node_id IS NOT NULL")?;
    add_from("SELECT next_node_type, next_node_id FROM teleports_item_nodes WHERE next_node_type IS NOT NULL AND next_node_id IS NOT NULL")?;
    add_from("SELECT next_node_type, next_node_id FROM teleports_ifslot_nodes WHERE next_node_type IS NOT NULL AND next_node_id IS NOT NULL")?;
    if table_exists(conn, "teleports_useOn_nodes")? {
        add_from("SELECT next_node_type, next_node_id FROM teleports_useOn_nodes WHERE next_node_type IS NOT NULL AND next_node_id IS NOT NULL")?;
    }
    Ok(set)
}

/// Chains that some other node points at are continuations, not starts.
type Incoming = HashSet<(NodeKind, i64)>;

fn enumerate_chain_starts(conn: &Connection, incoming: &Incoming) -> Result<Vec<(NodeKind, i64, (i32, i32, i32))>> {
    // Collect starting rows with concrete source positions (door, npc, object)
    let mut out: Vec<(NodeKind, i64, (i32, i32, i32))> = Vec::new();

    // Doors: src is outside tile
    {
        let mut st = conn.prepare_cached(
            "SELECT id, tile_outside_x, tile_outside_y, tile_outside_plane FROM teleports_door_nodes \
             WHERE tile_outside_x IS NOT NULL AND tile_outside_y IS NOT NULL AND tile_outside_plane IS NOT NULL \
             ORDER BY tile_outside_plane, tile_outside_y, tile_outside_x",
        )?;
        let rows = st.query_map([], |r: &Row| {
            let id: i64 = r.get(0)?;
            let x: i64 = r.get(1)?;
            let y: i64 = r.get(2)?;
            let p: i64 = r.get(3)?;
            Ok((id, (x as i32, y as i32, p as i32)))
        })?;
        for r in rows { let (id, pos) = r?; if !incoming.contains(&(NodeKind::Door, id)) { out.push((NodeKind::Door, id, pos)); } }
    }

    // NPCs: src is orig_min_*
    {
        let mut st = conn.prepare_cached(
            "SELECT id, orig_min_x, orig_min_y, orig_plane FROM teleports_npc_nodes \
             WHERE orig_min_x IS NOT NULL AND orig_min_y IS NOT NULL AND orig_plane IS NOT NULL \
             ORDER BY orig_plane, orig_min_y, orig_min_x",
        )?;
        let rows = st.query_map([], |r: &Row| {
            let id: i64 = r.get(0)?;
            let x: i64 = r.get(1)?;
            let y: i64 = r.get(2)?;
            let p: i64 = r.get(3)?;
            Ok((id, (x as i32, y as i32, p as i32)))
        })?;
        for r in rows { let (id, pos) = r?; if !incoming.contains(&(NodeKind::Npc, id)) { out.push((NodeKind::Npc, id, pos)); } }
    }

    // Objects: src is orig_min_*
    {
        let mut st = conn.prepare_cached(
            "SELECT id, orig_min_x, orig_min_y, orig_plane FROM teleports_object_nodes \
             WHERE orig_min_x IS NOT NULL AND orig_min_y IS NOT NULL AND orig_plane IS NOT NULL \
             ORDER BY orig_plane, orig_min_y, orig_min_x",
        )?;
        let rows = st.query_map([], |r: &Row| {
            let id: i64 = r.get(0)?;
            let x: i64 = r.get(1)?;
            let y: i64 = r.get(2)?;
            let p: i64 = r.get(3)?;
            Ok((id, (x as i32, y as i32, p as i32)))
        })?;
        for r in rows { let (id, pos) = r?; if !incoming.contains(&(NodeKind::Object, id)) { out.push((NodeKind::Object, id, pos)); } }
    }

    // useOn ("use item on object"): src is orig_min_*
    if table_exists(conn, "teleports_useOn_nodes")? {
        let mut st = conn.prepare_cached(
            "SELECT id, orig_min_x, orig_min_y, orig_plane FROM teleports_useOn_nodes \
             WHERE orig_min_x IS NOT NULL AND orig_min_y IS NOT NULL AND orig_plane IS NOT NULL \
             ORDER BY orig_plane, orig_min_y, orig_min_x",
        )?;
        let rows = st.query_map([], |r: &Row| {
            let id: i64 = r.get(0)?;
            let x: i64 = r.get(1)?;
            let y: i64 = r.get(2)?;
            let p: i64 = r.get(3)?;
            Ok((id, (x as i32, y as i32, p as i32)))
        })?;
        for r in rows { let (id, pos) = r?; if !incoming.contains(&(NodeKind::UseOn, id)) { out.push((NodeKind::UseOn, id, pos)); } }
    }

    Ok(out)
}

pub fn flatten_chains(
    conn: &Connection,
    _tiles: &[Tile],
    node_id_of: &NodeIndex,
) -> Result<Vec<MacroEdgeMeta>> {
    let incoming = collect_incoming_pairs(conn)?;
    flatten_chains_with(conn, node_id_of, &incoming)
}

/// Positional (macro-edge) and global chains together, scanning the incoming-pair set
/// once instead of once per flatten (the results are identical to calling
/// [`flatten_chains`] and [`flatten_global_chains`]).
pub fn flatten_all_chains(
    conn: &Connection,
    node_id_of: &NodeIndex,
) -> Result<(Vec<MacroEdgeMeta>, Vec<GlobalChainMeta>)> {
    let incoming = collect_incoming_pairs(conn)?;
    Ok((
        flatten_chains_with(conn, node_id_of, &incoming)?,
        flatten_global_chains_with(conn, node_id_of, &incoming)?,
    ))
}

fn flatten_chains_with(
    conn: &Connection,
    node_id_of: &NodeIndex,
    incoming: &Incoming,
) -> Result<Vec<MacroEdgeMeta>> {
    let mut result: Vec<MacroEdgeMeta> = Vec::new();

    let starts = enumerate_chain_starts(conn, incoming)?;

    for (start_kind, start_id, (sx, sy, sp)) in starts {
        // Map source tile to node id; skip if not present
        let Some(src_node) = node_id_of.get(sx, sy, sp) else { continue; };

        let mut visited: HashSet<(NodeKind, i64)> = HashSet::new();
        let mut steps: Vec<ChainStepMeta> = Vec::new();
        let mut requirement_ids: Vec<i64> = Vec::new();
        let mut cost_sum: f32 = 0.0;
        let mut cur_kind = start_kind;
        let mut cur_id = start_id;
        let mut last_dest: Option<(i32, i32, i32)> = None;
        // Capture first-door info to generate a reverse inside->outside edge for doors only
        let mut first_door_dest: Option<(i32, i32, i32)> = None;
        let mut first_door_reqs: Vec<i64> = Vec::new();
        let mut first_door_cost: f32 = 0.0;
        let mut first_door_id: Option<i64> = None;
        let mut cycle = false;

        loop {
            if !visited.insert((cur_kind, cur_id)) { cycle = true; break; }
            let Some(row) = fetch_step(conn, cur_kind, cur_id)? else { cycle = true; break; };
            cost_sum += row.cost;
            requirement_ids.extend(row.requirements.iter().copied());
            // Record the very first step details if this chain starts with a door
            if steps.is_empty() && start_kind == NodeKind::Door {
                if let Some(d) = row.dest { first_door_dest = Some(d); }
                first_door_reqs = row.requirements.clone();
                first_door_cost = row.cost;
                first_door_id = Some(cur_id);
            }
            steps.push(ChainStepMeta { kind: cur_kind.as_str(), id: cur_id, cost: row.cost, requirements: row.requirements, lodestone: row.lodestone });
            if let Some(d) = row.dest { last_dest = Some(d); }
            if let (Some(nk), Some(nid)) = (row.next_kind, row.next_id) {
                cur_kind = nk; cur_id = nid;
                continue;
            } else {
                break;
            }
        }
        if cycle { continue; } // drop cycles

        // Require a concrete destination discovered at some point in the chain
        let Some((dx, dy, dp)) = last_dest else { continue; };
        let Some(dst_node) = node_id_of.get(dx, dy, dp) else { continue; };

        // Dedup and sort req ids deterministically
        requirement_ids.sort_unstable();
        requirement_ids.dedup();

        result.push(MacroEdgeMeta {
            src: src_node,
            dst: dst_node,
            cost: cost_sum,
            requirement_ids,
            steps,
        });

        // Doors are bidirectional: add a reverse edge consisting solely of the first door step
        if start_kind == NodeKind::Door {
            if let Some((ix, iy, ip)) = first_door_dest {
                if let Some(inside_node) = node_id_of.get(ix, iy, ip) {
                    let rev_reqs: Vec<i64> = first_door_reqs.clone();
                    let door_id = first_door_id.unwrap_or(0);
                    result.push(MacroEdgeMeta {
                        src: inside_node,
                        dst: src_node,
                        cost: first_door_cost,
                        requirement_ids: rev_reqs,
                        steps: vec![ChainStepMeta { kind: NodeKind::Door.as_str(), id: door_id, cost: first_door_cost, requirements: first_door_reqs.clone(), lodestone: None }],
                    });
                }
            }
        }
    }

    // Deterministic ordering: by src, then dst, then cost, then steps len
    result.sort_by(|a, b| {
        a.src.cmp(&b.src)
            .then(a.dst.cmp(&b.dst))
            .then(a.cost.total_cmp(&b.cost))
            .then(a.steps.len().cmp(&b.steps.len()))
    });

    Ok(result)
}

#[derive(Debug, Clone)]
pub struct GlobalChainMeta {
    pub dst: u32,
    pub cost: f32,
    pub requirement_ids: Vec<i64>,
    pub steps: Vec<ChainStepMeta>,
}

fn enumerate_global_starts(conn: &Connection, incoming: &Incoming) -> Result<Vec<(NodeKind, i64)>> {
    let mut out: Vec<(NodeKind, i64)> = Vec::new();
    // Lodestones
    {
        let mut st = conn.prepare_cached("SELECT id FROM teleports_lodestone_nodes ORDER BY id")?;
        let rows = st.query_map([], |r: &Row| {
            let id: i64 = r.get(0)?;
            Ok(id)
        })?;
        for r in rows {
            let id = r?;
            if !incoming.contains(&(NodeKind::Lodestone, id)) { out.push((NodeKind::Lodestone, id)); }
        }
    }
    // Items
    {
        let mut st = conn.prepare_cached("SELECT id FROM teleports_item_nodes ORDER BY id")?;
        let rows = st.query_map([], |r: &Row| {
            let id: i64 = r.get(0)?;
            Ok(id)
        })?;
        for r in rows {
            let id = r?;
            if !incoming.contains(&(NodeKind::Item, id)) { out.push((NodeKind::Item, id)); }
        }
    }
    // Ifslots
    {
        let mut st = conn.prepare_cached("SELECT id FROM teleports_ifslot_nodes ORDER BY id")?;
        let rows = st.query_map([], |r: &Row| {
            let id: i64 = r.get(0)?;
            Ok(id)
        })?;
        for r in rows {
            let id = r?;
            if !incoming.contains(&(NodeKind::Ifslot, id)) { out.push((NodeKind::Ifslot, id)); }
        }
    }
    // POA items (standalone global teleports)
    {
        let mut st = conn.prepare_cached("SELECT id FROM teleports_POA_nodes ORDER BY id")?;
        let rows = st.query_map([], |r: &Row| {
            let id: i64 = r.get(0)?;
            Ok(id)
        })?;
        for r in rows {
            let id = r?;
            if !incoming.contains(&(NodeKind::Poa, id)) { out.push((NodeKind::Poa, id)); }
        }
    }
    Ok(out)
}

pub fn flatten_global_chains(
    conn: &Connection,
    node_id_of: &NodeIndex,
) -> Result<Vec<GlobalChainMeta>> {
    let incoming = collect_incoming_pairs(conn)?;
    flatten_global_chains_with(conn, node_id_of, &incoming)
}

fn flatten_global_chains_with(
    conn: &Connection,
    node_id_of: &NodeIndex,
    incoming: &Incoming,
) -> Result<Vec<GlobalChainMeta>> {
    let mut result: Vec<GlobalChainMeta> = Vec::new();
    let starts = enumerate_global_starts(conn, incoming)?;
    for (start_kind, start_id) in starts {
        let mut visited: HashSet<(NodeKind, i64)> = HashSet::new();
        let mut steps: Vec<ChainStepMeta> = Vec::new();
        let mut requirement_ids: Vec<i64> = Vec::new();
        let mut cost_sum: f32 = 0.0;
        let mut cur_kind = start_kind;
        let mut cur_id = start_id;
        let mut last_dest: Option<(i32, i32, i32)> = None;
        let mut cycle = false;

        loop {
            if !visited.insert((cur_kind, cur_id)) { cycle = true; break; }
            let Some(row) = fetch_step(conn, cur_kind, cur_id)? else { cycle = true; break; };
            cost_sum += row.cost;
            requirement_ids.extend(row.requirements.iter().copied());
            steps.push(ChainStepMeta { kind: cur_kind.as_str(), id: cur_id, cost: row.cost, requirements: row.requirements, lodestone: row.lodestone });
            if let Some(d) = row.dest { last_dest = Some(d); }
            if let (Some(nk), Some(nid)) = (row.next_kind, row.next_id) {
                cur_kind = nk; cur_id = nid;
                continue;
            } else {
                break;
            }
        }
        if cycle { continue; }
        let Some((dx, dy, dp)) = last_dest else { continue; };
        let Some(dst_node) = node_id_of.get(dx, dy, dp) else { continue; };

        requirement_ids.sort_unstable();
        requirement_ids.dedup();

        result.push(GlobalChainMeta { dst: dst_node, cost: cost_sum, requirement_ids, steps });
    }
    // Deterministic ordering by dst, then cost, then steps len
    result.sort_by(|a, b| a.dst.cmp(&b.dst)
        .then(a.cost.total_cmp(&b.cost))
        .then(a.steps.len().cmp(&b.steps.len())));
    Ok(result)
}
