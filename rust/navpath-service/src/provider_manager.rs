use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use navpath_core::{CostModel, Database};
use navpath_core::graph::provider::SqliteGraphProvider;
use tracing::info;

pub struct ProviderHandle {
    pub provider: Mutex<SqliteGraphProvider>,
}

impl ProviderHandle {
    fn new(db_path: &str) -> Result<Self> {
        let db = Database::open_read_only(db_path)?;
        let provider = SqliteGraphProvider::new(db, CostModel::default());
        Ok(Self { provider: Mutex::new(provider) })
    }
}

#[derive(Clone)]
pub struct ProviderManager {
    inner: Arc<Mutex<HashMap<String, Arc<ProviderHandle>>>>,
    default_db: Option<String>,
}

impl ProviderManager {
    pub fn new(default_db: Option<String>) -> Self {
        Self { inner: Arc::new(Mutex::new(HashMap::new())), default_db }
    }

    fn get_or_create_handle(&self, db_path: &str) -> Result<Arc<ProviderHandle>> {
        let mut guard = self.inner.lock().unwrap();
        if let Some(h) = guard.get(db_path) { return Ok(Arc::clone(h)); }
        info!(db_path=%db_path, "creating provider handle");
        let handle = Arc::new(ProviderHandle::new(db_path)?);
        guard.insert(db_path.to_string(), Arc::clone(&handle));
        Ok(handle)
    }

    pub fn with_provider<T, F>(&self, db_path: Option<&str>, f: F) -> Result<T>
    where
        F: FnOnce(&mut SqliteGraphProvider) -> Result<T>,
    {
        let path = db_path.map(|s| s.to_string()).or_else(|| self.default_db.clone())
            .ok_or_else(|| anyhow::anyhow!("no db_path provided and NAVPATH_DB not set"))?;
        let handle = self.get_or_create_handle(&path)?;
        let mut prov = handle.provider.lock().unwrap();
        f(&mut prov)
    }

    pub fn warm_default(&self) -> Result<()> {
        let path = self.default_db.clone().ok_or_else(|| anyhow::anyhow!("NAVPATH_DB not set"))?;
        let handle = self.get_or_create_handle(&path)?;
        let prov = handle.provider.lock().unwrap();
        prov.warm()?;
        Ok(())
    }
}
