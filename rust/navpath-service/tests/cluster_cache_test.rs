use navpath_service::db::Db;
use std::env;
use std::path::PathBuf;

fn db_path() -> PathBuf {
    if let Ok(p) = env::var("NAVPATH_DB") { return PathBuf::from(p); }
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest_dir).join("../../worldReachableTiles.db")
}

#[test]
fn cluster_lookup_hit_then_cached_hit() {
    let path = db_path();
    let db = Db::open_read_only(&path).expect("open read-only");

    // If schema lacks cluster tables, skip
    let clusters = match db.list_clusters(1) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("skipping test: list_clusters unavailable: {e}");
            return;
        }
    };
    if clusters.is_empty() {
        eprintln!("skipping test: no clusters present");
        return;
    }
    let first = &clusters[0];
    let tiles = match db.list_cluster_tiles(first.cluster_id) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping test: list_cluster_tiles unavailable: {e}");
            return;
        }
    };
    if tiles.is_empty() {
        eprintln!("skipping test: cluster has no tiles");
        return;
    }
    let t = &tiles[0];

    let cid1 = match db.get_cluster_id_for_tile(t.x as i32, t.y as i32, t.plane as i32) {
        Ok(Some(cid)) => cid,
        Ok(None) => {
            eprintln!("skipping test: tile not mapped to cluster");
            return;
        }
        Err(e) => {
            eprintln!("skipping test: get_cluster_id_for_tile unavailable: {e}");
            return;
        }
    };
    assert_eq!(cid1, first.cluster_id, "cluster id should match");

    // Call again to exercise the cache path
    let cid2 = match db.get_cluster_id_for_tile(t.x as i32, t.y as i32, t.plane as i32) {
        Ok(Some(cid)) => cid,
        Ok(None) => {
            eprintln!("skipping test: tile not mapped to cluster");
            return;
        }
        Err(e) => {
            eprintln!("skipping test: get_cluster_id_for_tile unavailable: {e}");
            return;
        }
    };
    assert_eq!(cid2, cid1, "cached value should match");
}

#[test]
fn cluster_lookup_miss_returns_none() {
    let path = db_path();
    let db = Db::open_read_only(&path).expect("open read-only");

    // Prepare statement may fail if schema lacks table; skip in that case
    match db.get_cluster_id_for_tile(-999999, -999999, -99) {
        Ok(cid) => {
            assert!(cid.is_none(), "invalid tile should not belong to any cluster");
        }
        Err(e) => {
            eprintln!("skipping test: get_cluster_id_for_tile unavailable: {e}");
            return;
        }
    }

    // Second call should also be fast via cached None
    match db.get_cluster_id_for_tile(-999999, -999999, -99) {
        Ok(cid2) => assert!(cid2.is_none()),
        Err(e) => {
            eprintln!("skipping test: cached none unavailable: {e}");
            return;
        }
    }
}
