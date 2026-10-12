//! Item catalog maintenance: resolve names on demand (cache-aside over the
//! item-info port) so search and the UI have human-readable labels.

use std::sync::Arc;

use crate::domain::NameRecord;
use crate::error::StoreError;
use crate::ports::{ItemCatalogRepository, ItemInfoPort};

pub struct CatalogService {
    catalog: Arc<dyn ItemCatalogRepository>,
    info: Arc<dyn ItemInfoPort>,
}

impl CatalogService {
    pub fn new(catalog: Arc<dyn ItemCatalogRepository>, info: Arc<dyn ItemInfoPort>) -> Self {
        Self { catalog, info }
    }

    /// Returns the catalog record, fetching it from the item API on first
    /// sight. A missing API record is remembered as a tombstone-less `None`
    /// for this call only (the next call retries — the item API is the
    /// authority and temporary failures must not stick).
    pub fn ensure(&self, item_id: i64) -> Result<Option<NameRecord>, StoreError> {
        if let Some(record) = self.catalog.by_id(item_id)? {
            return Ok(Some(record));
        }
        match self
            .info
            .describe(item_id)
            .map_err(|e| StoreError::Backend(e.to_string()))?
        {
            Some(record) => {
                self.catalog.upsert(std::slice::from_ref(&record))?;
                Ok(Some(record))
            }
            None => Ok(None),
        }
    }

    /// Substring search over known names (completions).
    pub fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<crate::domain::SearchHit>, StoreError> {
        self.catalog.search(query, limit)
    }
}
