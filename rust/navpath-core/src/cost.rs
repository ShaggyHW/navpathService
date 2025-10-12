use crate::models::Tile;
use crate::options::SearchOptions;

pub const DEFAULT_STEP_COST_MS: i64 = 2;
pub const DEFAULT_NODE_COST_MS: i64 = 6;

#[derive(Clone, Debug)]
pub struct CostModel {
    pub options: SearchOptions,
    pub step_cost_ms: i64,
}

impl Default for CostModel {
    fn default() -> Self {
        Self { options: SearchOptions::default(), step_cost_ms: DEFAULT_STEP_COST_MS }
    }
}

impl CostModel {
    pub fn new(options: SearchOptions) -> Self {
        Self { options, step_cost_ms: DEFAULT_STEP_COST_MS }
    }

    pub fn movement_cost(&self, _from: Tile, _to: Tile) -> i64 {
        self.step_cost_ms
    }

    pub fn door_cost(&self, db_cost: Option<i64>) -> i64 {
        self.with_override(self.options.door_cost_override, db_cost)
    }

    pub fn lodestone_cost(&self, db_cost: Option<i64>) -> i64 {
        self.with_override(self.options.lodestone_cost_override, db_cost)
    }

    pub fn object_cost(&self, db_cost: Option<i64>) -> i64 {
        self.with_override(self.options.object_cost_override, db_cost)
    }

    pub fn ifslot_cost(&self, db_cost: Option<i64>) -> i64 {
        self.with_override(self.options.ifslot_cost_override, db_cost)
    }

    pub fn npc_cost(&self, db_cost: Option<i64>) -> i64 {
        self.with_override(self.options.npc_cost_override, db_cost)
    }

    pub fn item_cost(&self, db_cost: Option<i64>) -> i64 {
        self.with_override(self.options.item_cost_override, db_cost)
    }

    pub fn heuristic(&self, current: Tile, goal: Tile) -> i64 {
        let opts = &self.options;
        if opts.use_lodestones
            || opts.use_objects
            || opts.use_ifslots
            || opts.use_npcs
            || opts.use_items
        {
            0
        } else {
            (Self::chebyshev_distance(current, goal) as i64) * self.step_cost_ms
        }
    }

    pub fn chebyshev_distance(a: Tile, b: Tile) -> i32 {
        let dx = (a[0] - b[0]).abs();
        let dy = (a[1] - b[1]).abs();
        dx.max(dy)
    }

    fn with_override(&self, override_cost: Option<i64>, db_value: Option<i64>) -> i64 {
        if let Some(o) = override_cost { return o; }
        if let Some(v) = db_value { return v; }
        DEFAULT_NODE_COST_MS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn movement_cost_is_constant_step_cost() {
        let cm = CostModel::default();
        assert_eq!(cm.movement_cost([0,0,0], [1,0,0]), DEFAULT_STEP_COST_MS);
        assert_eq!(cm.movement_cost([0,0,0], [1,1,0]), DEFAULT_STEP_COST_MS);
    }

    #[test]
    fn heuristic_is_chebyshev_when_only_movement_and_doors() {
        let mut opts = SearchOptions::default();
        // Disable all non-movement actions, leaving doors as in Python behavior
        opts.use_lodestones = false;
        opts.use_objects = false;
        opts.use_ifslots = false;
        opts.use_npcs = false;
        opts.use_items = false;
        let cm = CostModel::new(opts);
        // From (0,0) to (3,5): Chebyshev=5
        assert_eq!(cm.heuristic([0,0,0], [3,5,0]), 5 * DEFAULT_STEP_COST_MS);
    }

    #[test]
    fn heuristic_is_zero_when_any_actions_enabled() {
        let cm = CostModel::default();
        // Defaults enable actions, so heuristic should be 0
        assert_eq!(cm.heuristic([0,0,0], [10,10,0]), 0);
    }

    #[test]
    fn node_cost_overrides_and_db_values() {
        let mut opts = SearchOptions::default();
        opts.object_cost_override = None;
        let cm = CostModel::new(opts.clone());
        // No override, use db value, fallback default
        assert_eq!(cm.object_cost(Some(123)), 123);
        assert_eq!(cm.object_cost(None), DEFAULT_NODE_COST_MS);

        // With override
        opts.object_cost_override = Some(111);
        let cm2 = CostModel::new(opts);
        assert_eq!(cm2.object_cost(Some(999)), 111);
        assert_eq!(cm2.object_cost(None), 111);
    }
}
