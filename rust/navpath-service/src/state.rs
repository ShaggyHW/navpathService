use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use navpath_core::db::open::DbOpenConfig;

pub struct AppState {
    pub db_path: PathBuf,
    pub db_open_config: DbOpenConfig,
    pub ready: AtomicBool,
}

impl AppState {
    pub fn new(db_path: PathBuf, db_open_config: DbOpenConfig) -> Self {
        Self {
            db_path,
            db_open_config,
            ready: AtomicBool::new(false),
        }
    }
}

impl Clone for AppState {
    fn clone(&self) -> Self {
        Self {
            db_path: self.db_path.clone(),
            db_open_config: self.db_open_config.clone(),
            ready: AtomicBool::new(self.ready.load(Ordering::Relaxed)),
        }
    }
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("db_path", &self.db_path.display().to_string())
            .field("db_open_config", &self.db_open_config)
            .field("ready", &self.ready.load(Ordering::Relaxed))
            .finish()
    }
}
