use navpath_core::{astar::AStar, CostModel, SearchOptions, Tile};
use navpath_core::graph::provider::{Edge as GEdge, GraphProvider};

struct GridProvider;
impl GraphProvider for GridProvider {
    fn neighbors(&self, tile: Tile, _goal: Tile, _options: &SearchOptions) -> rusqlite::Result<Vec<GEdge>> {
        let [x,y,p] = tile;
        let mut edges = Vec::new();
        // 8-directional grid
        let moves: &[(i32,i32)] = &[
            (1,0), (-1,0), (0,1), (0,-1),
            (1,1), (1,-1), (-1,1), (-1,-1)
        ];
        for (dx,dy) in moves.iter().cloned() {
            let to = [x+dx, y+dy, p];
            edges.push(GEdge { type_: "move".into(), from_tile: tile, to_tile: to, cost_ms: 200, node: None, metadata: None });
        }
        Ok(edges)
    }
}

fn movement_only_defaults() -> SearchOptions {
    let mut opts = SearchOptions::default();
    opts.use_doors = false;
    opts.use_lodestones = false;
    opts.use_objects = false;
    opts.use_ifslots = false;
    opts.use_npcs = false;
    opts.use_items = false;
    opts
}

#[test]
fn jps_vs_legacy_on_diagonal_grid() {
    let provider = GridProvider;

    let opts_legacy = movement_only_defaults();
    let mut opts_jps = movement_only_defaults();
    opts_jps.extras.insert("jps_enabled".into(), serde_json::Value::from(true));

    let cm_legacy = CostModel::new(opts_legacy.clone());
    let cm_jps = CostModel::new(opts_jps.clone());

    let astar_legacy = AStar::new(&provider, &cm_legacy);
    let astar_jps = AStar::new(&provider, &cm_jps);

    let start = [0,0,0];
    let goal = [10,10,0];

    let res_legacy = astar_legacy.find_path(start, goal, &opts_legacy).expect("legacy path");
    let res_jps = astar_jps.find_path(start, goal, &opts_jps).expect("jps path");

    // Both should succeed
    assert!(res_legacy.path.is_some());
    assert!(res_jps.path.is_some());

    // Paths should be equal or very similar; costs equal in this grid model
    assert_eq!(res_legacy.path, res_jps.path);
    assert_eq!(res_legacy.cost_ms, res_jps.cost_ms);

    // JPS should not expand more nodes than legacy in an open diagonal grid
    assert!(res_jps.expanded <= res_legacy.expanded, "JPS expanded {} > legacy {}", res_jps.expanded, res_legacy.expanded);
}
