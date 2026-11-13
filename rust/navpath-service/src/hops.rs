//! Surge and Dive: where the game lands a hop, and when a simple caster would cast one
//! along a route's walk.
//!
//! **Trajectory** (`greedy` / `ray` / `facing`): a port of the launcher's
//! `HopModel.greedy`, `HopModel.ray` and `HopModel.facing` (Cluetrainer's `dive_internal`
//! / `dive_far_internal`). A hop does not fly a straight line: each step takes the
//! diagonal toward the target if that diagonal step is walkable, else the x step, else
//! the y step, never leaving the rectangle spanned by start and target, so a hop slides
//! around corners. No step possible = the hop is refused. Dive = `greedy` to a tile
//! within [`HOP_RANGE`]; Surge = `ray`, the furthest tile 10..1 along one of 8 directions
//! that `greedy` reaches. "Walkable step" is the snapshot's walk edge (the builder's
//! `walk_mask` + `walk_edge_ok`), so it reproduces ~94 % of real hops (see the launcher's
//! docs/surge_dive_improvements, §0-§4).
//!
//! **Schedule** ([`plan_walk`]): a deliberately simple, slightly pessimistic caster —
//! none of the client's holds, pair/chain bonuses, end-of-walk scheduling or learned hop
//! knowledge. Along a walk, at each tile reached, it casts the usable hop with the
//! largest gain (route tiles skipped; ties go to Surge), or walks on:
//! - gain >= [`SURGE_MIN_GAIN`] / [`DIVE_MIN_GAIN`], and no hop with fewer than
//!   [`END_GATE_TILES`] walk tiles left;
//! - Dive aims at the later walk tile within [`HOP_RANGE`] (Chebyshev) with the best gain
//!   that `greedy` reaches; Surge goes along the facing (the last step walked, or the
//!   last hop's facing) and must land on a later walk tile. No facing (first move after
//!   the route start, a teleport or an interaction) = no Surge;
//! - off cooldown plus a [`REACTION_MS`] reaction, and [`LANDING_HOLD_MS`] after a
//!   teleport / interaction landing; a Surge also puts every charge on the game's 2-tick
//!   anti-spam delay ([`SURGE_SPAM_MS`]);
//! - a hop takes one tick ([`HOP_MS`]); walking resumes from the landing.
//!
//! The reaction runs while the character keeps walking (the caster never stands still
//! to wait for an ability), so it never delays the walk: every hop costs [`HOP_MS`].

/// Reach of Surge and Dive (tiles, Chebyshev).
pub const HOP_RANGE: i32 = 10;
/// Least route tiles a Surge must skip to be worth its charge.
pub const SURGE_MIN_GAIN: usize = 8;
/// Least route tiles a Dive must skip.
pub const DIVE_MIN_GAIN: usize = 6;
/// No hop with fewer than this many walk tiles left. (Implied by the minimum gains
/// today — a hop must land on the walk — but kept explicit, as the client has it.)
pub const END_GATE_TILES: usize = 5;
/// Human reaction between an ability coming off cooldown and its cast.
pub const REACTION_MS: f64 = 700.0;
/// No hop this soon after a teleport or interaction landing.
pub const LANDING_HOLD_MS: f64 = 1200.0;
/// A hop takes one server tick.
pub const HOP_MS: f64 = 600.0;
/// After a Surge, every Surge charge waits 2 ticks (the game's anti-spam delay).
pub const SURGE_SPAM_MS: f64 = 1200.0;
/// How far along the walk (route tiles) a Dive aim or a Surge landing is looked for.
/// A Dive aim must also be within [`HOP_RANGE`]; the window only bounds the scan on
/// winding walks (the client looks 120 tiles on, to find a fence's far side).
pub const HOP_LOOKAHEAD: usize = 120;
/// One walk step (running: 2 tiles per 600 ms tick, diagonal or not).
pub const WALK_STEP_MS: f64 = navpath_core::snapshot::WALK_CARDINAL_MS as f64;

pub type Tile = (i32, i32, i32);

/// May a size-1 mover step from (x, y, plane) by (dx, dy) (each -1, 0 or 1)?
pub trait StepRule {
    fn can_step(&mut self, x: i32, y: i32, plane: i32, dx: i32, dy: i32) -> bool;
}

/// Direction bit of a unit step in the builder's mask order (graph.rs `DIR_DELTA`):
/// 0 W, 1 S, 2 E, 3 N, 4 NW, 5 SW, 6 SE, 7 NE.
#[inline]
fn dir_bit(dx: i32, dy: i32) -> Option<u8> {
    Some(match (dx, dy) {
        (-1, 0) => 0,
        (0, -1) => 1,
        (1, 0) => 2,
        (0, 1) => 3,
        (-1, 1) => 4,
        (-1, -1) => 5,
        (1, -1) => 6,
        (1, 1) => 7,
        _ => return None,
    })
}

/// [`StepRule`] over the snapshot's walk graph: a step is legal iff the walk edge
/// exists (degree <= 8). Each tile's 8-direction mask is derived once per request.
pub struct SnapshotSteps<'a> {
    snap: &'a navpath_core::Snapshot,
    masks: crate::FxHashMap<Tile, u8>,
}

impl<'a> SnapshotSteps<'a> {
    pub fn new(snap: &'a navpath_core::Snapshot) -> Self {
        Self { snap, masks: crate::FxHashMap::default() }
    }

    fn mask(&mut self, x: i32, y: i32, p: i32) -> u8 {
        let snap = self.snap;
        *self.masks.entry((x, y, p)).or_insert_with(|| {
            let Some(u) = snap.find_node(x, y, p) else { return 0 };
            let offs = snap.walk_offsets();
            let dst = snap.walk_dst();
            let (s, e) = (offs[u as usize] as usize, offs[u as usize + 1] as usize);
            let mut m = 0u8;
            for &v in &dst[s..e] {
                let (vx, vy, vp) = snap.node_coord(v);
                if vp == p {
                    if let Some(b) = dir_bit(vx - x, vy - y) {
                        m |= 1 << b;
                    }
                }
            }
            m
        })
    }
}

impl StepRule for SnapshotSteps<'_> {
    fn can_step(&mut self, x: i32, y: i32, plane: i32, dx: i32, dy: i32) -> bool {
        match dir_bit(dx, dy) {
            Some(b) => self.mask(x, y, plane) & (1 << b) != 0,
            None => false,
        }
    }
}

/// `HopModel.greedy`: walk from `from` to `target` by the hop rule (diagonal toward the
/// target if legal, else the x step, else the y step, inside the start/target
/// rectangle). True when `target` is reached, false when a step has no legal choice.
/// No range check.
pub fn greedy<R: StepRule + ?Sized>(rule: &mut R, from: Tile, target: Tile) -> bool {
    if from.2 != target.2 {
        return false;
    }
    let (mut x, mut y, p) = from;
    let (tx, ty) = (target.0, target.1);
    let (sx, sy) = ((tx - x).signum(), (ty - y).signum());
    let (min_x, max_x, min_y, max_y) = (x.min(tx), x.max(tx), y.min(ty), y.max(ty));
    let inside = |x: i32, y: i32| x >= min_x && x <= max_x && y >= min_y && y <= max_y;
    while x != tx || y != ty {
        let (dx, dy) = if sx != 0 && sy != 0 && inside(x + sx, y + sy) && rule.can_step(x, y, p, sx, sy) {
            (sx, sy)
        } else if sx != 0 && inside(x + sx, y) && rule.can_step(x, y, p, sx, 0) {
            (sx, 0)
        } else if sy != 0 && inside(x, y + sy) && rule.can_step(x, y, p, 0, sy) {
            (0, sy)
        } else {
            return false;
        };
        x += dx;
        y += dy;
    }
    true
}

/// `HopModel.ray`: the furthest tile `range..1` along direction `d` that [`greedy`]
/// reaches; None when not even one is (a hop of 0 tiles).
pub fn ray<R: StepRule + ?Sized>(rule: &mut R, from: Tile, d: (i32, i32), range: i32) -> Option<Tile> {
    if d == (0, 0) {
        return None;
    }
    (1..=range).rev().map(|i| (from.0 + d.0 * i, from.1 + d.1 * i, from.2)).find(|&t| greedy(rule, from, t))
}

/// `HopModel.facing`: the way the player faces after moving (dx, dy) in one go — the
/// 8-way direction nearest its angle (0 N, 90 E), which a Surge cast straight after
/// goes along. None for (0, 0).
pub fn facing(dx: i32, dy: i32) -> Option<(i32, i32)> {
    if dx == 0 && dy == 0 {
        return None;
    }
    let mut deg = (dx as f64).atan2(dy as f64).to_degrees();
    if deg < 0.0 {
        deg += 360.0;
    }
    let sector = (((deg % 360.0) + 22.5) / 45.0).floor() as i32 % 8;
    Some(match sector {
        0 => (0, 1),
        1 => (1, 1),
        2 => (1, 0),
        3 => (1, -1),
        4 => (0, -1),
        5 => (-1, -1),
        6 => (-1, 0),
        _ => (-1, 1),
    })
}

/// Whether `t` is one of the tiles 1..=[`HOP_RANGE`] along `dir` from `from`.
#[inline]
fn on_ray(from: Tile, dir: (i32, i32), t: Tile) -> bool {
    if t.2 != from.2 {
        return false;
    }
    let (dx, dy) = (t.0 - from.0, t.1 - from.1);
    let k = if dir.0 != 0 { dx * dir.0 } else { dy * dir.1 };
    (1..=HOP_RANGE).contains(&k) && dx == dir.0 * k && dy == dir.1 * k
}

/// Ability inputs of one request (cooldowns/charges come from the caller).
#[derive(Debug, Clone, Copy)]
pub struct Abilities {
    pub surge: bool,
    pub surge_charges: u32,
    pub surge_cooldown_ms: f64,
    pub dive: bool,
    /// When the Dive comes off cooldown, relative to the route's start.
    pub dive_available_in_ms: f64,
    pub dive_cooldown_ms: f64,
}

impl Abilities {
    pub fn any(&self) -> bool {
        (self.surge && self.surge_charges > 0) || self.dive
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HopKind {
    Surge,
    Dive,
}

/// One hop cast by [`plan_walk`]: from walk index `from` to walk index `to`, at
/// `cast_ms` on the route clock.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hop {
    pub kind: HopKind,
    pub from: usize,
    pub to: usize,
    pub cast_ms: f64,
}

/// The caster's state along a whole route: the route clock, cooldowns, the facing and
/// the last landing. Owned by the route's action pass; walks are fed through
/// [`HopState::plan_walk`], everything else through [`HopState::other_action`].
pub struct HopState {
    pub abilities: Abilities,
    /// Route clock (ms since the first action).
    pub elapsed: f64,
    /// When each Surge charge is (was) off cooldown.
    surge_ready: Vec<f64>,
    /// No Surge before this (anti-spam after the last one).
    surge_spam_until: f64,
    dive_ready: f64,
    /// Unit direction the character faces (None = unknown).
    facing: Option<(i32, i32)>,
    /// End of the last teleport / interaction landing (None = none yet: the route start
    /// itself is not a landing).
    landed_at: Option<f64>,
}

impl HopState {
    pub fn new(abilities: Abilities) -> Self {
        let charges = if abilities.surge { abilities.surge_charges as usize } else { 0 };
        Self {
            abilities,
            elapsed: 0.0,
            surge_ready: vec![0.0; charges],
            surge_spam_until: f64::NEG_INFINITY,
            dive_ready: abilities.dive_available_in_ms.max(0.0),
            facing: None,
            landed_at: None,
        }
    }

    /// A non-walk action (teleport, door, NPC, ...) of `cost_ms`: the clock advances,
    /// the character lands (hold) with an unknown facing.
    pub fn other_action(&mut self, cost_ms: f64) {
        self.elapsed += cost_ms;
        self.landed_at = Some(self.elapsed);
        self.facing = None;
    }

    fn landing_clear(&self, t: f64) -> bool {
        self.landed_at.is_none_or(|l| t >= l + LANDING_HOLD_MS)
    }

    fn surge_charge(&self, t: f64) -> Option<usize> {
        if t < self.surge_spam_until {
            return None;
        }
        self.surge_ready.iter().position(|&r| t >= r + REACTION_MS)
    }

    fn dive_usable(&self, t: f64) -> bool {
        self.abilities.dive && t >= self.dive_ready + REACTION_MS
    }

    /// Plan one walk: `tiles[0]` is where the character stands, `tiles[1..]` the walked
    /// tiles in order (all on one plane, each a walk step from the previous). Advances
    /// the clock over the walk, casting hops on the way; returns them in order. Every
    /// tile not skipped by a hop is walked at [`WALK_STEP_MS`].
    pub fn plan_walk<R: StepRule + ?Sized>(&mut self, rule: &mut R, tiles: &[Tile]) -> Vec<Hop> {
        let mut hops = Vec::new();
        if tiles.len() < 2 {
            return hops;
        }
        let end = tiles.len() - 1;
        let mut i = 0usize;
        loop {
            if self.abilities.any() && end - i >= END_GATE_TILES {
                if let Some(hop) = self.try_cast(rule, tiles, i, end) {
                    self.elapsed += HOP_MS;
                    i = hop.to;
                    hops.push(hop);
                    continue;
                }
            }
            if i == end {
                break;
            }
            let (a, b) = (tiles[i], tiles[i + 1]);
            self.facing = Some(((b.0 - a.0).signum(), (b.1 - a.1).signum()));
            self.elapsed += WALK_STEP_MS;
            i += 1;
        }
        hops
    }

    fn try_cast<R: StepRule + ?Sized>(&mut self, rule: &mut R, tiles: &[Tile], i: usize, end: usize) -> Option<Hop> {
        let t = self.elapsed;
        if !self.landing_clear(t) {
            return None;
        }
        let here = tiles[i];
        let last = end.min(i + HOP_LOOKAHEAD);

        let surge = match (self.surge_charge(t), self.facing) {
            // Cheap pre-check before the ray's up-to-100 step tests: the landing must be
            // a walk tile at least SURGE_MIN_GAIN on that lies on the ray.
            (Some(charge), Some(dir)) if (i + SURGE_MIN_GAIN..=last).any(|j| on_ray(here, dir, tiles[j])) => {
                ray(rule, here, dir, HOP_RANGE)
                    .and_then(|land| (i + 1..=last).find(|&j| tiles[j] == land))
                    .filter(|&j| j - i >= SURGE_MIN_GAIN)
                    .map(|j| (j, charge))
            }
            _ => None,
        };
        let dive = if self.dive_usable(t) {
            (i + DIVE_MIN_GAIN..=last).rev().find(|&j| {
                let aim = tiles[j];
                aim.2 == here.2
                    && (aim.0 - here.0).abs().max((aim.1 - here.1).abs()) <= HOP_RANGE
                    && greedy(rule, here, aim)
            })
        } else {
            None
        };

        let hop = match (surge, dive) {
            (Some((sj, charge)), dj) if dj.is_none_or(|dj| sj >= dj) => {
                self.surge_ready[charge] = t + self.abilities.surge_cooldown_ms;
                self.surge_spam_until = t + SURGE_SPAM_MS;
                // The facing is unchanged: a Surge goes the way the character faces.
                Hop { kind: HopKind::Surge, from: i, to: sj, cast_ms: t }
            }
            (_, Some(dj)) => {
                self.dive_ready = t + self.abilities.dive_cooldown_ms;
                let to = tiles[dj];
                self.facing = facing(to.0 - here.0, to.1 - here.1);
                Hop { kind: HopKind::Dive, from: i, to: dj, cast_ms: t }
            }
            _ => return None,
        };
        Some(hop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Open ground with walls: `blocked_edges` holds directed (x, y, dx, dy) steps that
    /// are refused (both ways are added by `wall`), `solid` tiles cannot be entered. A
    /// diagonal also needs both of its cardinal L-paths open (the builder's rule).
    #[derive(Default)]
    struct Grid {
        solid: HashSet<(i32, i32)>,
        blocked_edges: HashSet<(i32, i32, i32, i32)>,
    }

    impl Grid {
        fn wall(&mut self, x: i32, y: i32, dx: i32, dy: i32) {
            self.blocked_edges.insert((x, y, dx, dy));
            self.blocked_edges.insert((x + dx, y + dy, -dx, -dy));
        }
        fn card(&self, x: i32, y: i32, dx: i32, dy: i32) -> bool {
            !self.solid.contains(&(x + dx, y + dy)) && !self.blocked_edges.contains(&(x, y, dx, dy))
        }
    }

    impl StepRule for Grid {
        fn can_step(&mut self, x: i32, y: i32, _p: i32, dx: i32, dy: i32) -> bool {
            if dx == 0 || dy == 0 {
                return self.card(x, y, dx, dy);
            }
            self.card(x, y, dx, dy)
                && self.card(x, y, dx, 0)
                && self.card(x, y, 0, dy)
                && self.card(x + dx, y, 0, dy)
                && self.card(x, y + dy, dx, 0)
        }
    }

    fn t(x: i32, y: i32) -> Tile {
        (x, y, 0)
    }

    #[test]
    fn greedy_reaches_open_targets() {
        let mut g = Grid::default();
        assert!(greedy(&mut g, t(0, 0), t(7, 3)));
        assert!(greedy(&mut g, t(0, 0), t(-4, -9)));
        assert!(!greedy(&mut g, t(0, 0), (3, 3, 1)), "never across planes");
    }

    #[test]
    fn greedy_slides_around_a_corner() {
        // The NE diagonal out of (2,2) is refused (a solid corner tile at (3,2)
        // blocks the L-path), so the hop takes the y step, then continues diagonally.
        let mut g = Grid::default();
        g.solid.insert((3, 2));
        assert!(!g.can_step(2, 2, 0, 1, 1));
        assert!(greedy(&mut g, t(0, 0), t(5, 6)), "corner slide: blocked diagonal -> y step, then on");
        // Blocked diagonal AND blocked x step: the y step still carries it.
        let mut g = Grid::default();
        g.wall(0, 0, 1, 0);
        assert!(greedy(&mut g, t(0, 0), t(1, 4)));
    }

    #[test]
    fn greedy_refuses_when_no_step_is_possible() {
        // Target due east behind a wall: no diagonal (dy = 0), no y step, x refused.
        let mut g = Grid::default();
        g.wall(3, 0, 1, 0);
        assert!(!greedy(&mut g, t(0, 0), t(6, 0)));
        // Never leaves the rectangle: a wall across the whole box blocks, even though
        // walking around it is possible.
        let mut g = Grid::default();
        for y in 0..=3 {
            g.wall(2, y, 1, 0);
        }
        assert!(!greedy(&mut g, t(0, 0), t(5, 3)));
    }

    #[test]
    fn ray_stops_at_the_furthest_reachable_tile() {
        let mut g = Grid::default();
        assert_eq!(ray(&mut g, t(0, 0), (1, 0), HOP_RANGE), Some(t(10, 0)));
        assert_eq!(ray(&mut g, t(0, 0), (1, 1), HOP_RANGE), Some(t(10, 10)));
        g.wall(6, 0, 1, 0);
        assert_eq!(ray(&mut g, t(0, 0), (1, 0), HOP_RANGE), Some(t(6, 0)), "a cardinal surge stops at the wall");
        let mut g = Grid::default();
        g.wall(0, 0, 1, 0);
        assert_eq!(ray(&mut g, t(0, 0), (1, 0), HOP_RANGE), None, "no tile at all");
        // A diagonal surge slides past a corner and still lands on the ray.
        let mut g = Grid::default();
        g.solid.insert((4, 3));
        assert_eq!(ray(&mut g, t(0, 0), (1, 1), HOP_RANGE), Some(t(10, 10)));
    }

    #[test]
    fn on_ray_matches_the_ray_tiles() {
        for dir in [(1, 0), (0, -1), (1, 1), (-1, 1)] {
            for k in 1..=HOP_RANGE {
                assert!(on_ray(t(5, 5), dir, t(5 + dir.0 * k, 5 + dir.1 * k)));
            }
            assert!(!on_ray(t(5, 5), dir, t(5, 5)));
            assert!(!on_ray(t(5, 5), dir, t(5 + dir.0 * 11, 5 + dir.1 * 11)));
            assert!(!on_ray(t(5, 5), dir, t(5 - dir.0, 5 - dir.1)));
        }
        assert!(!on_ray(t(0, 0), (1, 1), t(3, 2)));
        assert!(!on_ray(t(0, 0), (1, 0), t(3, 1)));
        assert!(!on_ray(t(0, 0), (1, 0), (3, 0, 1)));
    }

    #[test]
    fn facing_is_the_nearest_octant() {
        assert_eq!(facing(10, 0), Some((1, 0)));
        assert_eq!(facing(10, 5), Some((1, 1)));
        assert_eq!(facing(10, 4), Some((1, 0)));
        assert_eq!(facing(-3, -3), Some((-1, -1)));
        assert_eq!(facing(4, -8), Some((1, -1)));
        assert_eq!(facing(0, 0), None);
    }

    fn line(n: i32, d: (i32, i32)) -> Vec<Tile> {
        (0..=n).map(|i| t(d.0 * i, d.1 * i)).collect()
    }

    fn abilities(surge: bool, dive: bool) -> Abilities {
        Abilities {
            surge,
            surge_charges: 2,
            surge_cooldown_ms: 10_200.0,
            dive,
            dive_available_in_ms: 0.0,
            dive_cooldown_ms: 10_200.0,
        }
    }

    #[test]
    fn no_hops_without_abilities_and_the_walk_costs_300_a_tile() {
        let mut st = HopState::new(abilities(false, false));
        assert!(st.plan_walk(&mut Grid::default(), &line(20, (1, 0))).is_empty());
        assert_eq!(st.elapsed, 20.0 * 300.0);
        let mut st = HopState::new(abilities(false, false));
        st.plan_walk(&mut Grid::default(), &line(20, (1, 1)));
        assert_eq!(st.elapsed, 20.0 * 300.0, "diagonal steps cost 300 too");
    }

    #[test]
    fn dive_waits_for_the_reaction_then_takes_the_best_gain() {
        let mut st = HopState::new(abilities(false, true));
        let hops = st.plan_walk(&mut Grid::default(), &line(20, (1, 0)));
        // Ready at 0 + 700 ms reaction: cast at the first tile reached at >= 700 ms
        // (tile 3, 900 ms), aimed 10 tiles on.
        assert_eq!(hops, vec![Hop { kind: HopKind::Dive, from: 3, to: 13, cast_ms: 900.0 }]);
        // 3 walked + hop + 7 walked.
        assert_eq!(st.elapsed, 10.0 * 300.0 + HOP_MS);
    }

    #[test]
    fn minimum_gains() {
        // 8 tiles: a dive gains at most 8 - 3 = 5 < 6.
        let mut st = HopState::new(abilities(true, true));
        assert!(st.plan_walk(&mut Grid::default(), &line(8, (1, 0))).is_empty());
        // 9 tiles: the dive gains exactly 6.
        let mut st = HopState::new(abilities(false, true));
        let hops = st.plan_walk(&mut Grid::default(), &line(9, (1, 0)));
        assert_eq!(hops.len(), 1);
        assert_eq!(hops[0].to - hops[0].from, 6);
        // A surge needs 8. The walk heads east, then turns north at a solid tile; the
        // surge from tile 3 (the first after the reaction) stops at the corner.
        let turn_at = |cx: i32| {
            let mut tiles = line(cx, (1, 0));
            tiles.extend((1..=15).map(|i| t(cx, i)));
            let mut g = Grid::default();
            g.solid.insert((cx + 1, 0));
            (tiles, g)
        };
        let (tiles, mut g) = turn_at(10); // gain 7
        let mut st = HopState::new(abilities(true, false));
        assert!(st.plan_walk(&mut g, &tiles).iter().all(|h| h.from != 3), "a 7-tile surge is not cast");
        let (tiles, mut g) = turn_at(11); // gain 8
        let mut st = HopState::new(abilities(true, false));
        let hops = st.plan_walk(&mut g, &tiles);
        assert_eq!(hops[0], Hop { kind: HopKind::Surge, from: 3, to: 11, cast_ms: 900.0 });
    }

    #[test]
    fn end_gate() {
        // A walk of 30 that already used its dive: no hop is cast with < 5 tiles left,
        // even when every ability is ready (the gains rule it out too).
        let mut st = HopState::new(abilities(true, true));
        let hops = st.plan_walk(&mut Grid::default(), &line(30, (1, 0)));
        for h in &hops {
            assert!(30 - h.from >= END_GATE_TILES);
        }
        let mut st = HopState::new(abilities(true, true));
        st.elapsed = 5_000.0;
        assert!(st.plan_walk(&mut Grid::default(), &line(4, (1, 0))).is_empty());
    }

    #[test]
    fn no_surge_without_a_facing() {
        // After a landing the facing is unknown: the first step has to be walked. With a
        // ready surge and no hold left, the surge goes from tile 1, not tile 0.
        let mut st = HopState::new(abilities(true, false));
        st.elapsed = 10_000.0;
        st.other_action(0.0);
        st.elapsed += LANDING_HOLD_MS; // the hold is over before the walk starts
        let hops = st.plan_walk(&mut Grid::default(), &line(20, (1, 0)));
        assert_eq!(hops[0].from, 1);
        assert_eq!(hops[0].kind, HopKind::Surge);
    }

    #[test]
    fn a_dive_gives_the_surge_its_facing() {
        // A surge and a dive chain: the second hop goes straight from the first landing.
        let mut st = HopState::new(abilities(true, true));
        let hops = st.plan_walk(&mut Grid::default(), &line(40, (1, 0)));
        assert!(hops.len() >= 2);
        assert_eq!(hops[0].kind, HopKind::Surge, "ties and longer gains go to the surge");
        // The second hop starts right where the first landed (chained, no walk between).
        assert_eq!(hops[1].from, hops[0].to);
        assert_eq!(hops[1].kind, HopKind::Dive);
        // And after a lone dive from a standstill, the surge uses the dive's facing.
        let mut st = HopState::new(abilities(true, true));
        st.surge_ready = vec![1e9, 1e9]; // surge busy at first
        let hops = st.plan_walk(&mut Grid::default(), &line(12, (1, 1)));
        assert_eq!(hops[0].kind, HopKind::Dive);
        assert_eq!(st.facing, Some((1, 1)));
    }

    #[test]
    fn cooldown_spacing_with_two_surge_charges() {
        let mut st = HopState::new(abilities(true, false));
        let hops = st.plan_walk(&mut Grid::default(), &line(200, (1, 0)));
        let surges: Vec<f64> = hops.iter().map(|h| h.cast_ms).collect();
        assert!(surges.len() >= 4, "{surges:?}");
        // Two charges: the first two casts are only the anti-spam delay apart...
        assert!(surges[1] - surges[0] >= SURGE_SPAM_MS && surges[1] - surges[0] < 2_000.0, "{surges:?}");
        // ...then each charge waits its own cooldown (+ reaction).
        assert!(surges[2] - surges[0] >= 10_200.0 + REACTION_MS, "{surges:?}");
        assert!(surges[3] - surges[1] >= 10_200.0 + REACTION_MS, "{surges:?}");
        for w in surges.windows(2) {
            assert!(w[1] - w[0] >= SURGE_SPAM_MS);
        }
    }

    #[test]
    fn no_hop_within_the_landing_hold() {
        let mut st = HopState::new(abilities(true, true));
        st.elapsed = 50_000.0;
        st.other_action(3_000.0); // a teleport lands at 53,000
        let hops = st.plan_walk(&mut Grid::default(), &line(30, (1, 0)));
        assert!(hops[0].cast_ms >= 53_000.0 + LANDING_HOLD_MS);
        assert_eq!(hops[0].from, 4, "1,200 ms of walking = 4 tiles");
    }

    #[test]
    fn surge_must_land_on_the_walk() {
        // The walk turns north after 6 tiles. An east surge would land off the route, so
        // none goes before the turn; after it the facing is north and one does.
        let mut tiles = line(6, (1, 0));
        tiles.extend((1..=12).map(|i| t(6, i)));
        let mut st = HopState::new(abilities(true, false));
        let hops = st.plan_walk(&mut Grid::default(), &tiles);
        assert_eq!(hops.len(), 1);
        assert_eq!((hops[0].from, tiles[hops[0].to]), (7, t(6, 11)));
        // The dive takes the corner instead, from the first tile it is usable on.
        let mut st = HopState::new(abilities(false, true));
        let hops = st.plan_walk(&mut Grid::default(), &tiles);
        assert_eq!(hops[0].kind, HopKind::Dive);
        assert_eq!(hops[0].from, 3);
        assert_eq!(tiles[hops[0].to], t(6, 10), "aims at the furthest walk tile within 10 (Chebyshev)");
    }
}
