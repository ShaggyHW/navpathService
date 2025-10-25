use std::env;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub db_path: Option<PathBuf>,
    pub move_cost_ms: Option<u64>,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let host = env::var("NAVPATH_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
        let port = env::var("NAVPATH_PORT")
            .ok()
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(8080);
        let db_path = env::var("NAVPATH_DB").ok().map(PathBuf::from);
        let move_cost_ms = env::var("NAVPATH_MOVE_COST_MS").ok().and_then(|s| s.parse::<u64>().ok());

        Ok(Self {
            host,
            port,
            db_path,
            move_cost_ms,
        })
    }
}
