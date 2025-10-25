use navpath_service::models::Tile;
use navpath_service::planner::micro_astar::find_path_4dir;

fn allow_rect(x: i32, y: i32, min_x: i32, max_x: i32, min_y: i32, max_y: i32) -> bool {
    x >= min_x && x <= max_x && y >= min_y && y <= max_y
}

#[test]
fn path_straight_line_no_diagonals() {
    let start = Tile { x: 0, y: 0, plane: 0 };
    let goal = Tile { x: 3, y: 0, plane: 0 };
    let is_allowed = |x: i32, y: i32| allow_rect(x, y, 0, 3, 0, 3);
    let is_walkable = |_x: i32, _y: i32| true;

    let path = find_path_4dir(start, goal, is_allowed, is_walkable).expect("path");
    // Expect a straight line
    assert_eq!(path, vec![
        Tile { x: 0, y: 0, plane: 0 },
        Tile { x: 1, y: 0, plane: 0 },
        Tile { x: 2, y: 0, plane: 0 },
        Tile { x: 3, y: 0, plane: 0 },
    ]);
}

#[test]
fn path_avoids_blocked_and_respects_bounds() {
    let start = Tile { x: 0, y: 0, plane: 0 };
    let goal = Tile { x: 2, y: 0, plane: 0 };
    let is_allowed = |x: i32, y: i32| allow_rect(x, y, 0, 3, 0, 3);
    // Block a vertical wall at x=1 for y=0..=1 forcing a detour
    let is_walkable = |x: i32, y: i32| !((x == 1 && (y == 0 || y == 1)));

    let path = find_path_4dir(start, goal, is_allowed, is_walkable).expect("path");
    // Verify starts/ends correct
    assert_eq!(path.first().copied(), Some(start));
    assert_eq!(path.last().copied(), Some(goal));
    // Verify all steps 4-neighbor moves (no diagonals) and within bounds and not on blocked cells
    for w in path.windows(2) {
        let a = w[0];
        let b = w[1];
        let dx = (a.x - b.x).abs();
        let dy = (a.y - b.y).abs();
        assert!(dx + dy == 1, "only 4-neighbor moves allowed");
        assert!(is_allowed(b.x, b.y));
        assert!(is_walkable(b.x, b.y));
    }
}

#[test]
fn returns_none_when_not_allowed_or_not_walkable() {
    let start = Tile { x: 0, y: 0, plane: 0 };
    let goal = Tile { x: 1, y: 0, plane: 0 };
    let is_allowed = |_x: i32, _y: i32| false; // disallow all
    let is_walkable = |_x: i32, _y: i32| true;
    assert!(find_path_4dir(start, goal, is_allowed, is_walkable).is_none());

    let is_allowed2 = |_x: i32, _y: i32| true;
    let is_walkable2 = |_x: i32, _y: i32| false; // block all
    assert!(find_path_4dir(start, goal, is_allowed2, is_walkable2).is_none());
}
