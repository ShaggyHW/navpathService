use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use navpath_core::db::open::DbOpenConfig;
use crate::config::JpsMode;

pub struct AppState {
    pub db_path: PathBuf,
    pub db_open_config: DbOpenConfig,
    pub ready: AtomicBool,
    pub jps_mode: JpsMode,
}

impl AppState {
    pub fn new(db_path: PathBuf, db_open_config: DbOpenConfig, jps_mode: JpsMode) -> Self {
        Self {
            db_path,
            db_open_config,
            ready: AtomicBool::new(false),
            jps_mode,
        }
    }
}

impl Clone for AppState {
    fn clone(&self) -> Self {
        Self {
            db_path: self.db_path.clone(),
            db_open_config: self.db_open_config.clone(),
            ready: AtomicBool::new(self.ready.load(Ordering::Relaxed)),
            jps_mode: self.jps_mode,
        }
    }
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("db_path", &self.db_path.display().to_string())
            .field("db_open_config", &self.db_open_config)
            .field("ready", &self.ready.load(Ordering::Relaxed))
            .field("jps_mode", &self.jps_mode)
            .finish()
    }
}
