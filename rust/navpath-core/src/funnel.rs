/// Funnel (string-pulling) algorithm that converts an ordered portal corridor
/// into world-space waypoints.
///
/// Deterministic, allocation-free except for output waypoints.
/// No external dependencies.
#[inline]
pub fn string_pull(
    start: [f64; 2],
    portals: &[( [f64; 2], [f64; 2] )],
    goal: [f64; 2],
    eps: f64,
) -> Vec<[f64; 2]> {
    // If no portals, return straight line
    if portals.is_empty() {
        return vec![start, goal];
    }

    // Helper for twice signed area
    #[inline]
    fn area2(a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> f64 {
        (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
    }

    // Ensure first portal is ordered (left, right) relative to start apex.
    #[inline]
    fn ordered_lr(apex: [f64; 2], l: [f64; 2], r: [f64; 2]) -> ([f64; 2], [f64; 2]) {
        if (r[0] - apex[0]) * (l[1] - apex[1]) - (r[1] - apex[1]) * (l[0] - apex[0]) > 0.0 {
            (l, r)
        } else {
            (r, l)
        }
    }

    let mut res: Vec<[f64; 2]> = Vec::with_capacity(portals.len() + 2);
    res.push(start);

    let mut apex = start;
    let mut last_apex = [f64::NAN, f64::NAN];
    // Orient initial portal properly
    let (mut left, mut right) = ordered_lr(apex, portals[0].0, portals[0].1);
    let mut left_idx = 0usize;
    let mut right_idx = 0usize;

    // Iterate through all portals and finally a degenerate portal at the goal.
    let mut i = 1usize;
    while i <= portals.len() {
        let (pl, pr) = if i < portals.len() { portals[i] } else { (goal, goal) };
        // Ensure orientation relative to current apex
        let (pl, pr) = ordered_lr(apex, pl, pr);

        // Tighten right edge
        if area2(apex, right, pr) <= eps {
            // New right
            if area2(apex, left, pr) < -eps {
                // Crossed over left - append left, advance apex
                res.push(left);
                apex = left;
                // Reset funnel
                i = left_idx + 1;
                right_idx = left_idx;
                left = apex;
                right = apex;
                continue;
            }
            right = pr;
            right_idx = i;
        }

        // Tighten left edge
        if area2(apex, left, pl) >= -eps {
            // New left
            if area2(apex, right, pl) > eps {
                // Crossed over right - append left cusp and advance apex (deterministic choice)
                res.push(left);
                last_apex = apex;
                apex = left;
                // Reset funnel
                i = left_idx + 1;
                right_idx = left_idx;
                left = apex;
                right = apex;
                // Safety: if apex didn't change due to numeric eps, break to avoid infinite loop
                if (apex[0] - last_apex[0]).abs() <= eps && (apex[1] - last_apex[1]).abs() <= eps { break; }
                continue;
            }
            left = pl;
            left_idx = i;
        }

        i += 1;
    }

    // Append goal if it's not equal to last
    if let Some(last) = res.last() {
        if (last[0] - goal[0]).abs() > eps || (last[1] - goal[1]).abs() > eps {
            res.push(goal);
        }
    } else {
        res.push(goal);
    }

    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn straight_corridor_returns_straight_line() {
        let start = [0.0, 0.0];
        let portals = vec![([1.0, -1.0], [1.0, 1.0]), ([2.0, -1.0], [2.0, 1.0])];
        let goal = [3.0, 0.0];
        let wp = string_pull(start, &portals, goal, 1e-12);
        assert_eq!(wp.len(), 2);
        assert!((wp[0][0] - 0.0).abs() < 1e-12 && (wp[1][0] - 3.0).abs() < 1e-12);
    }

    #[test]
    fn corridor_with_turn_adds_corner() {
        let start = [0.0, 0.0];
        // First portal vertical around x=1 with top narrower, then shift upwards
        let portals = vec![([1.0, -0.5], [1.0, 0.5]), ([2.0, 0.5], [2.0, 2.0])];
        let goal = [3.0, 1.5];
        let wp = string_pull(start, &portals, goal, 1e-12);
        assert!(wp.len() >= 3);
        // Expect a corner near [1.0, 0.5]
        let corner = wp[1];
        assert!((corner[0] - 1.0).abs() < 1e-9);
        assert!((corner[1] - 0.5).abs() < 1e-9);
        // Final point is goal
        let last = *wp.last().unwrap();
        assert!((last[0] - goal[0]).abs() < 1e-12 && (last[1] - goal[1]).abs() < 1e-12);
    }

    #[test]
    fn single_portal_degenerate_orders() {
        let start = [0.0, 0.0];
        // Provide portal in reverse order; algorithm should reorder
        let portals = vec![([1.0, 0.1], [1.0, -0.1])];
        let goal = [2.0, 0.0];
        let wp = string_pull(start, &portals, goal, 1e-12);
        assert_eq!(wp.len(), 2);
        // Path directly to goal
        let last = *wp.last().unwrap();
        assert!((last[0] - goal[0]).abs() < 1e-12 && (last[1] - goal[1]).abs() < 1e-12);
    }
}
