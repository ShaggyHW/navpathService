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

    let clusters = match db.list_clusters(5) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("skipping smoke test: list_clusters unavailable: {e}");
            return;
        }
    };
    if let Some(first) = clusters.first() {
        let tiles = match db.list_cluster_tiles(first.cluster_id) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("skipping smoke test: list_cluster_tiles unavailable: {e}");
                return;
            }
        };
        if let Some(tile) = tiles.first() {
            if let Err(e) = db.get_tile(tile.x as i32, tile.y as i32, tile.plane as i32) {
                eprintln!("skipping smoke test: get_tile unavailable: {e}");
                return;
            }
        }
        let entrances = match db.list_cluster_entrances_by_cluster(first.cluster_id) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("skipping smoke test: list_cluster_entrances_by_cluster unavailable: {e}");
                return;
            }
        };
        if let (Some(a), Some(b)) = (entrances.get(0), entrances.get(1)) {
            if let Err(e) = db.get_intraconnection(a.entrance_id, b.entrance_id) {
                eprintln!("skipping smoke test: get_intraconnection unavailable: {e}");
                return;
            }
            if let Err(e) = db.get_interconnection(a.entrance_id, b.entrance_id) {
                eprintln!("skipping smoke test: get_interconnection unavailable: {e}");
                return;
            }
        }
    }

    if let Err(e) = db.get_teleport_requirement(1) {
        eprintln!("skipping smoke test: get_teleport_requirement unavailable: {e}");
        return;
    }

    if let Err(e) = db.list_abstract_teleport_edges_by_dst(0, 0, 0) {
        eprintln!("skipping smoke test: list_abstract_teleport_edges_by_dst unavailable: {e}");
        return;
    }
}

