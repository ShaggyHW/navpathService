use std::env;
use std::net::SocketAddr;

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
