use navpath_service::db::{Db, Cluster};
use std::env;
use std::path::PathBuf;

fn db_path() -> PathBuf {
    if let Ok(p) = env::var("NAVPATH_DB") {
        return PathBuf::from(p);
    }
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest_dir).join("../../worldReachableTiles.db")
}

#[test]
fn open_db_and_query_clusters() {
    let path = db_path();
    let db = Db::open_read_only(&path).expect("open read-only");

    let clusters = db.list_clusters(5).expect("list clusters");
    if let Some(first) = clusters.first() {
        let tiles = db
            .list_cluster_tiles(first.cluster_id)
            .expect("list cluster tiles for first cluster");
        if let Some(tile) = tiles.first() {
            let _maybe_tile = db
                .get_tile(tile.x as i32, tile.y as i32, tile.plane as i32)
                .expect("get tile");
        }
        let entrances = db
            .list_cluster_entrances_by_cluster(first.cluster_id)
            .expect("list cluster entrances");
        if let (Some(a), Some(b)) = (entrances.get(0), entrances.get(1)) {
            let _intra = db
                .get_intraconnection(a.entrance_id, b.entrance_id)
                .expect("get intraconnection (may be None)");
            let _inter = db
                .get_interconnection(a.entrance_id, b.entrance_id)
                .expect("get interconnection (may be None)");
        }
    }

    let _req = db
        .get_teleport_requirement(1)
        .expect("get_teleport_requirement (may be None)");

    let _edges = db
        .list_abstract_teleport_edges_by_dst(0, 0, 0)
        .expect("list abstract teleport edges by dst (may be empty)");
}
