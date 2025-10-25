use navpath_service::models::Tile;
use navpath_service::planner::path_blob::{try_decode_default, JsonPathBlobDecoder, PathBlobDecoder, try_decode_with_decoders};

#[test]
fn decode_array_of_arrays_ok() {
    let blob = br"[[1,2,3],[2,2,3],[3,2,3]]";
    let out = try_decode_default(blob).expect("should decode");
    assert_eq!(out, vec![
        Tile { x: 1, y: 2, plane: 3 },
        Tile { x: 2, y: 2, plane: 3 },
        Tile { x: 3, y: 2, plane: 3 },
    ]);
}

#[test]
fn decode_array_of_objects_ok() {
    let blob = br#"[{"x":1,"y":2,"plane":3},{"x":2,"y":2,"plane":3}]"#;
    let out = try_decode_default(blob).expect("should decode");
    assert_eq!(out, vec![
        Tile { x: 1, y: 2, plane: 3 },
        Tile { x: 2, y: 2, plane: 3 },
    ]);
}

#[test]
fn decode_invalid_returns_none() {
    let blob = b"not json";
    assert!(try_decode_default(blob).is_none());

    // invalid UTF-8
    let blob = &[0xff, 0xfe, 0xfd];
    let dec = JsonPathBlobDecoder;
    assert!(dec.decode(blob).is_none());
}

#[test]
fn decode_uses_first_successful_decoder() {
    struct Never;
    impl PathBlobDecoder for Never { fn decode(&self, _blob: &[u8]) -> Option<Vec<Tile>> { None } }
    let good = JsonPathBlobDecoder;
    let blob = br"[[4,5,6]]";
    let out = try_decode_with_decoders(blob, [&Never as &dyn PathBlobDecoder, &good]);
    assert_eq!(out, Some(vec![Tile{ x:4, y:5, plane:6}]));
}
