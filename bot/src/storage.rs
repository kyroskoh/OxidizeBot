use anyhow::Error;
use std::{path::Path, sync::Arc};

pub use futures_cache::{sled, Cache};

pub struct Storage {
    db: Arc<sled::Db>,
}

impl Storage {
    /// Open the given storage location.
    pub fn open(path: &Path) -> Result<Storage, Error> {
        let db = sled::open(path.join("sled.30"))?;
        Ok(Storage { db: Arc::new(db) })
    }

    /// Access the cache abstraction of your storage.
    pub fn cache(&self) -> Result<Cache, Error> {
        Ok(Cache::load(Arc::new(self.db.open_tree("cache")?))?)
    }
}
