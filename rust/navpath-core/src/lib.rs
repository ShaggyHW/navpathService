//! navpath-core: core library
//!
//! Exposes data transfer objects (DTOs) for interoperable JSON shapes
//! and core utilities used by the service crate.

pub mod json;
pub mod models;
pub mod options;
pub mod cost;
pub mod db;
pub mod graph;
pub mod nodes;
pub mod astar;
pub mod jps;
pub mod geometry;
pub mod funnel;

pub use cost::CostModel;
pub use models::{ActionStep, NodeRef, PathResult, Rect, Tile};
pub use options::SearchOptions;
pub use db::Database;
pub use jps::JpsConfig;
pub use crate::graph::navmesh_provider::NavmeshGraphProvider;

/// Returns the crate version for basic linkage diagnostics.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_non_empty() {
        assert!(!version().is_empty());
    }

    #[test]
    fn exports_available() {
        // Simple smoke test that re-exports are accessible
        let _tile: Tile = [0, 0, 0];
        let _nr = NodeRef { type_: "door".into(), id: 1 };
        let _pr = PathResult { path: None, actions: vec![], reason: None, expanded: 0, cost_ms: 0 };
        let _opts = SearchOptions::default();
        let _cm = CostModel::default();
        let _ = (_tile, _nr, _pr, _opts, _cm);
    }
}
