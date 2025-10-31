use crate::db::Db;
use crate::models::Tile;
use crate::planner::micro_astar::find_path_4dir;
use crate::planner::graph::GraphInputs;
use crate::planner::high_level::plan_hl_indices;
use crate::planner::hpa::{HpaOptions, reconstruct_tiles_and_actions};
use crate::requirements::RequirementEvaluator;
use anyhow::Result;
use std::collections::{HashMap, HashSet};

/// Try a same-cluster micro A* if start and end resolve to the same cluster_id.
/// Returns Ok(Some(path_tiles)) when a constrained path is found.
/// Returns Ok(None) when clusters differ, missing, or no path within the cluster.
pub fn plan_same_cluster<F>(db: &Db, start: Tile, end: Tile, is_walkable: F) -> Result<Option<Vec<Tile>>>
where
    F: Fn(i32, i32, i32) -> bool,
{
    if start.plane != end.plane {
        return Ok(None);
    }

    let cid_s = match db.get_cluster_id_for_tile(start.x, start.y, start.plane) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    let cid_e = match db.get_cluster_id_for_tile(end.x, end.y, end.plane) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    let (Some(cid_s), Some(cid_e)) = (cid_s, cid_e) else { return Ok(None) };
    if cid_s != cid_e { return Ok(None) }

    // Build allowed set for this cluster
    let tiles = match db.list_cluster_tiles(cid_s) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    if tiles.is_empty() {
        return Ok(None);
    }
    let plane = start.plane;
    let allowed_set: HashSet<(i32, i32)> = tiles
        .into_iter()
        .filter(|t| t.plane as i32 == plane)
        .map(|t| (t.x as i32, t.y as i32))
        .collect();

    let is_allowed = |x: i32, y: i32| allowed_set.contains(&(x, y));
    let is_walk = |x: i32, y: i32| is_walkable(x, y, plane);

    Ok(find_path_4dir(start, end, is_allowed, is_walk))
}

#[derive(Debug, Clone)]
pub struct ClusterPlanResult {
    pub path: Vec<Tile>,
    pub actions: Vec<serde_json::Value>,
}

/// Cluster-aware planner that assembles micro bridges for entrance hops.
/// Returns Ok(Some(result)) when a path is found; Ok(None) when no route is possible.
pub fn plan_cluster_aware(
    db: &Db,
    evaluator: &RequirementEvaluator,
    start: Tile,
    end: Tile,
    is_walkable: &dyn Fn(i32, i32, i32) -> bool,
) -> Result<Option<ClusterPlanResult>> {
    // Fast path: same-cluster constrained micro A*
    if let Some(path) = plan_same_cluster(db, start, end, |x, y, p| is_walkable(x, y, p))? {
        return Ok(Some(ClusterPlanResult { path, actions: Vec::new() }));
    }

    // Gather abstract graph inputs for planes of interest
    let plane_s = start.plane;
    let plane_e = end.plane;

    let mut entrances = db.list_cluster_entrances_by_plane(plane_s).unwrap_or_default();
    if plane_e != plane_s {
        let mut more = db.list_cluster_entrances_by_plane(plane_e).unwrap_or_default();
        entrances.append(&mut more);
    }
    if entrances.is_empty() { return Ok(None); }

    let mut intra = db.list_cluster_intraconnections_by_plane(plane_s).unwrap_or_default();
    if plane_e != plane_s {
        let mut more = db.list_cluster_intraconnections_by_plane(plane_e).unwrap_or_default();
        intra.append(&mut more);
    }
    let mut inter = db.list_cluster_interconnections_by_plane(plane_s).unwrap_or_default();
    if plane_e != plane_s {
        let mut more = db.list_cluster_interconnections_by_plane(plane_e).unwrap_or_default();
        inter.append(&mut more);
    }
    let teleports = db.list_abstract_teleport_edges_for_planes(plane_s, plane_e).unwrap_or_default();
    let teleport_requirements = db.list_teleport_requirements().unwrap_or_default();

    // Build cluster tiles map for all clusters referenced by entrances
    let mut cluster_tiles: HashMap<i64, HashSet<(i32, i32, i32)>> = HashMap::new();
    for e in entrances.iter() {
        let cid = e.cluster_id;
        if !cluster_tiles.contains_key(&cid) {
            if let Ok(tiles) = db.list_cluster_tiles(cid) {
                let mut set = HashSet::new();
                for t in tiles {
                    set.insert((t.x as i32, t.y as i32, t.plane as i32));
                }
                cluster_tiles.insert(cid, set);
            }
        }
    }

    let graph_inputs = GraphInputs {
        entrances: &entrances,
        intra: &intra,
        inter: &inter,
        teleports: &teleports,
        teleport_requirements: &teleport_requirements,
    };

    // Compute high-level hop indices
    let boxed_walk: Box<dyn Fn(i32, i32, i32) -> bool> = Box::new(move |x, y, p| is_walkable(x, y, p));
    let (graph, hl_indices) = match plan_hl_indices(&graph_inputs, evaluator, start, end, &cluster_tiles, &boxed_walk) {
        Some(v) => v,
        None => return Ok(None),
    };

    // Reconstruct tiles and actions using shared HPA utilities
    let opts = HpaOptions { start, end };
    let (path, actions) = reconstruct_tiles_and_actions(&graph, &hl_indices, &opts, &cluster_tiles, &boxed_walk);

    Ok(Some(ClusterPlanResult { path, actions }))
}
