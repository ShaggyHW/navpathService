use std::env;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub db_path: Option<PathBuf>,
    pub move_cost_ms: Option<u64>,
    pub debug_result_path: Option<PathBuf>,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let host = env::var("NAVPATH_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
        let port = env::var("NAVPATH_PORT")
            .ok()
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(8080);
        let db_path = env::var("NAVPATH_DB")
            .ok()
            .map(PathBuf::from)
            .or_else(|| Some(PathBuf::from("/home/query/Dev/rs3cache_extractor/worldReachableTiles.db")));
        let move_cost_ms = env::var("NAVPATH_MOVE_COST_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .or(Some(600));
        let debug_result_path = env::var("NAVPATH_DEBUG_RESULT_PATH")
            .ok()
            .map(PathBuf::from)
            .or_else(|| Some(PathBuf::from("/home/query/Dev/navpathService/result.json")));

        Ok(Self {
            host,
            port,
            db_path,
            move_cost_ms,
            debug_result_path,
        })
    }
}
