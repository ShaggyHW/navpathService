use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::bail;
use navpath_core::db::open::DbOpenConfig;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderMode {
    Sqlite,
    Navmesh,
}

impl Default for ProviderMode {
    fn default() -> Self { ProviderMode::Sqlite }
}

pub fn provider_mode_from_env() -> ProviderMode {
    match env::var("NAVPATH_PROVIDER").ok().map(|s| s.to_ascii_lowercase()) {
        Some(ref s) if s == "navmesh" => ProviderMode::Navmesh,
        Some(ref s) if s == "sqlite" => ProviderMode::Sqlite,
        _ => ProviderMode::Navmesh,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JpsMode {
    Auto,
    Off,
}

impl Default for JpsMode {
    fn default() -> Self { JpsMode::Auto }
}

pub fn jps_mode_from_env() -> JpsMode {
    match env::var("NAVPATH_JPS_MODE").ok().map(|s| s.to_ascii_lowercase()) {
        Some(ref s) if s == "off" => JpsMode::Off,
        Some(ref s) if s == "auto" => JpsMode::Auto,
        _ => JpsMode::Auto,
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub addr: SocketAddr,
    pub default_db: Option<String>,
}

impl Config {
    pub fn from_env() -> Self {
        let host = env::var("NAVPATH_HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
        let port: u16 = env::var("NAVPATH_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(8080);
        let addr: SocketAddr = format!("{}:{}", host, port).parse().expect("invalid host/port");
        let default_db = env::var("NAVPATH_DB").ok();
        Self { addr, default_db }
    }
}

pub fn resolve_db_path() -> anyhow::Result<PathBuf> {
    let mut args = env::args().skip(1);
    let mut cli: Option<String> = None;
    while let Some(arg) = args.next() {
        if arg == "--db" {
            if let Some(p) = args.next() { cli = Some(p); }
            break;
        } else if let Some(rest) = arg.strip_prefix("--db=") {
            cli = Some(rest.to_string());
            break;
        }
    }
    let chosen = if let Some(p) = cli { p } else if let Ok(p) = env::var("NAVPATH_DB") { p } else { bail!("database path not provided: use --db <PATH> or set NAVPATH_DB") };
    if chosen.trim().is_empty() { bail!("database path is empty"); }
    let pb = PathBuf::from(chosen);
    if !pb.is_absolute() { bail!("database path must be absolute"); }
    Ok(pb)
}

pub fn load_db_open_config() -> DbOpenConfig {
    DbOpenConfig::from_env()
}
