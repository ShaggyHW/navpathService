use navpath_service::models::Tile;
use navpath_service::serialization::{serialize_path, move_action, lodestone_action};

#[test]
fn serialize_path_as_array_of_int_triplets() {
    let path = vec![
        Tile { x: 1, y: 2, plane: 3 },
        Tile { x: 4, y: 5, plane: 6 },
    ];
    let arr = serialize_path(&path);
    assert_eq!(arr, vec![[1,2,3],[4,5,6]]);
}

#[test]
fn move_action_shape_matches() {
    let from = Tile { x: 3067, y: 3505, plane: 0 };
    let to = Tile { x: 3068, y: 3505, plane: 0 };
    let v = move_action(from, to, 200);
    // Check essential fields and integer preservation
    assert_eq!(v.get("type").and_then(|x| x.as_str()), Some("move"));
    assert_eq!(v["from"]["min"], serde_json::json!([3067,3505,0]));
    assert_eq!(v["to"]["min"], serde_json::json!([3068,3505,0]));
    assert_eq!(v["cost_ms"].as_i64(), Some(200));
}

#[test]
fn lodestone_action_shape_matches_sample() {
    // Based on result.txt first action structure
    let from = Tile { x: 2900, y: 3537, plane: 0 };
    let to = Tile { x: 3067, y: 3505, plane: 0 };
    let v = lodestone_action(from, to, 17000, 13, "EDGEVILLE", "EDGEVILLE", Some(8));

    assert_eq!(v.get("type").and_then(|x| x.as_str()), Some("lodestone"));
    // from/to bounds
    assert_eq!(v["from"]["min"], serde_json::json!([2900,3537,0]));
    assert_eq!(v["from"]["max"], serde_json::json!([2900,3537,0]));
    assert_eq!(v["to"]["min"], serde_json::json!([3067,3505,0]));
    assert_eq!(v["to"]["max"], serde_json::json!([3067,3505,0]));
    // cost
    assert_eq!(v["cost_ms"].as_i64(), Some(17000));
    // node ref
    assert_eq!(v["node"]["type"], serde_json::json!("lodestone"));
    assert_eq!(v["node"]["id"], serde_json::json!(13));
    // metadata.db_row echo
    let db_row = &v["metadata"]["db_row"];
    assert_eq!(db_row["id"], serde_json::json!(13));
    assert_eq!(db_row["lodestone"], serde_json::json!("EDGEVILLE"));
    assert_eq!(db_row["dest"], serde_json::json!([3067,3505,0]));
    assert_eq!(db_row["cost"], serde_json::json!(17000));
    assert!(db_row["next_node_type"].is_null());
    assert!(db_row["next_node_id"].is_null());
    assert_eq!(db_row["requirement_id"], serde_json::json!(8));
}

