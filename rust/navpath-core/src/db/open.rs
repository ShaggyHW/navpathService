use std::env;
use std::fmt::{Display, Formatter};
use std::path::Path;

use rusqlite::{Connection, OpenFlags};

/// Safe toggles for SQLite PRAGMAs and open behavior suitable for RO workloads.
#[derive(Clone, Debug)]
pub struct DbOpenConfig {
    /// If true, set PRAGMA query_only=ON.
    pub query_only: bool,
    /// If Some(kb) and kb > 0, set PRAGMA cache_size = -kb (KB units).
    pub cache_size_kb: Option<i64>,
    /// If Some(bytes) and bytes > 0, set PRAGMA mmap_size = bytes.
    pub mmap_size_bytes: Option<i64>,
    /// If Some, set PRAGMA temp_store accordingly.
    pub temp_store: Option<TempStore>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TempStore { Memory, File }

impl Display for TempStore {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self { TempStore::Memory => write!(f, "MEMORY"), TempStore::File => write!(f, "FILE"), }
    }
}

impl Default for DbOpenConfig {
    fn default() -> Self {
        Self {
            query_only: true,
            cache_size_kb: Some(200_000), // ~200MB
            mmap_size_bytes: Some(268_435_456), // 256MB
            temp_store: Some(TempStore::Memory),
        }
    }
}

impl DbOpenConfig {
    /// Load toggles from environment variables. Missing/invalid values fall back to defaults.
    ///
    /// Variables:
    /// - NAVPATH_SQLITE_QUERY_ONLY: "1"/"0" (default 1)
    /// - NAVPATH_SQLITE_CACHE_SIZE_KB: integer KB; 0 disables
    /// - NAVPATH_SQLITE_MMAP_SIZE: integer bytes; 0 disables
    /// - NAVPATH_SQLITE_TEMP_STORE: "MEMORY" or "FILE"; empty disables
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(v) = env::var("NAVPATH_SQLITE_QUERY_ONLY") { cfg.query_only = v != "0"; }
        if let Ok(v) = env::var("NAVPATH_SQLITE_CACHE_SIZE_KB") {
            match v.parse::<i64>() { Ok(n) if n > 0 => cfg.cache_size_kb = Some(n), _ => cfg.cache_size_kb = None }
        }
        if let Ok(v) = env::var("NAVPATH_SQLITE_MMAP_SIZE") {
            match v.parse::<i64>() { Ok(n) if n > 0 => cfg.mmap_size_bytes = Some(n), _ => cfg.mmap_size_bytes = None }
        }
        if let Ok(v) = env::var("NAVPATH_SQLITE_TEMP_STORE") {
            let vv = v.trim().to_ascii_uppercase();
            cfg.temp_store = match vv.as_str() {
                "MEMORY" => Some(TempStore::Memory),
                "FILE" => Some(TempStore::File),
                _ => None,
            };
        }
        cfg
    }
}

/// Open a SQLite database read-only and apply safe PRAGMAs based on the provided config.
/// Falls back gracefully if certain flags are unsupported. All PRAGMA errors are ignored.
pub fn open_read_only_with_config<P: AsRef<Path>>(path: P, cfg: &DbOpenConfig) -> rusqlite::Result<Connection> {
    let path_ref = path.as_ref();
    // Try most restrictive first: READ_ONLY + URI
    let attempt1 = Connection::open_with_flags(path_ref, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI);
    let conn = match attempt1 {
        Ok(c) => c,
        Err(_e1) => {
            // Try READ_ONLY without URI support
            match Connection::open_with_flags(path_ref, OpenFlags::SQLITE_OPEN_READ_ONLY) {
                Ok(c2) => c2,
                Err(_e2) => {
                    // Last resort: normal open; enforce query_only via PRAGMA below
                    Connection::open(path_ref)?
                }
            }
        }
    };

    apply_pragmas(&conn, cfg);
    Ok(conn)
}

fn apply_pragmas(conn: &Connection, cfg: &DbOpenConfig) {
    // Always enable FK checks (safe in RO)
    let _ = conn.execute("PRAGMA foreign_keys = ON", []);
    if cfg.query_only { let _ = conn.execute("PRAGMA query_only = ON", []); }
    if let Some(kb) = cfg.cache_size_kb { if kb > 0 { let _ = conn.execute(&format!("PRAGMA cache_size = -{}", kb), []); } }
    if let Some(bytes) = cfg.mmap_size_bytes { if bytes > 0 { let _ = conn.execute(&format!("PRAGMA mmap_size = {}", bytes), []); } }
    if let Some(ts) = cfg.temp_store { let _ = conn.execute(&format!("PRAGMA temp_store = {}", ts), []); }
}
