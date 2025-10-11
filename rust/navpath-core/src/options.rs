use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

pub const DEFAULT_MAX_EXPANSIONS: u64 = 1_000_000; // per perf doc
pub const DEFAULT_TIMEOUT_MS: u64 = 5_000; // per perf doc
pub const DEFAULT_MAX_CHAIN_DEPTH: u32 = 5_000; // matches Python

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchOptions {
    pub use_doors: bool,
    pub use_lodestones: bool,
    pub use_objects: bool,
    pub use_ifslots: bool,
    pub use_npcs: bool,
    pub use_items: bool,

    pub max_expansions: u64,
    pub timeout_ms: u64,
    pub max_chain_depth: u32,

    pub door_cost_override: Option<i64>,
    pub lodestone_cost_override: Option<i64>,
    pub object_cost_override: Option<i64>,
    pub ifslot_cost_override: Option<i64>,
    pub npc_cost_override: Option<i64>,
    pub item_cost_override: Option<i64>,

    #[serde(default)]
    pub extras: HashMap<String, Value>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            use_doors: true,
            use_lodestones: true,
            use_objects: true,
            use_ifslots: true,
            use_npcs: true,
            use_items: true,
            max_expansions: DEFAULT_MAX_EXPANSIONS,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_chain_depth: DEFAULT_MAX_CHAIN_DEPTH,
            door_cost_override: Some(600),
            lodestone_cost_override: Some(17000),
            object_cost_override: Some(2000),
            ifslot_cost_override: Some(1000),
            npc_cost_override: Some(1000),
            item_cost_override: Some(3000),
            extras: HashMap::new(),
        }
    }
}

impl SearchOptions {
    /// Returns true when only movement edges are enabled (all action edges disabled).
    pub fn movement_only(&self) -> bool {
        !(self.use_doors
            || self.use_lodestones
            || self.use_objects
            || self.use_ifslots
            || self.use_npcs
            || self.use_items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_match_perf_doc() {
        let o = SearchOptions::default();
        assert_eq!(o.max_expansions, DEFAULT_MAX_EXPANSIONS);
        assert_eq!(o.timeout_ms, DEFAULT_TIMEOUT_MS);
        assert_eq!(o.max_chain_depth, DEFAULT_MAX_CHAIN_DEPTH);
        assert!(o.use_doors && o.use_lodestones && o.use_objects && o.use_ifslots && o.use_npcs && o.use_items);
    }

    #[test]
    fn movement_only_detects_correctly() {
        let mut o = SearchOptions::default();
        assert!(!o.movement_only());
        o.use_doors = false;
        o.use_lodestones = false;
        o.use_objects = false;
        o.use_ifslots = false;
        o.use_npcs = false;
        o.use_items = false;
        assert!(o.movement_only());
    }

    #[test]
    fn deserializes_with_defaults_when_missing_fields() {
        // Only provide one field; all others should take defaults
        let v = json!({ "use_doors": false });
        let o: SearchOptions = serde_json::from_value(v).unwrap();
        assert_eq!(o.use_doors, false);
        // Some defaulted fields spot-check
        assert_eq!(o.use_lodestones, true);
        assert_eq!(o.max_expansions, DEFAULT_MAX_EXPANSIONS);
        assert_eq!(o.max_chain_depth, DEFAULT_MAX_CHAIN_DEPTH);
        assert!(o.extras.is_empty());
    }
}
