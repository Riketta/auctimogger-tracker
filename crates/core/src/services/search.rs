//! Name/id search over the item catalog (completions feed off the same port).

use std::sync::Arc;

use crate::{
    domain::SearchHit,
    error::StoreError,
    ports::ItemCatalogRepository,
};

/// Name/id search over the item catalog (completions feed off the same port).
pub struct SearchService {
    catalog: Arc<dyn ItemCatalogRepository>,
}

impl SearchService {
    /// Creates the service over the catalog repository.
    pub fn new(catalog: Arc<dyn ItemCatalogRepository>) -> Self {
        Self { catalog }
    }

    /// Prefix-first search; `limit` bounds the completion list.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the catalog cannot be read.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<SearchHit>, StoreError> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        self.catalog.search(query, limit.max(1), offset)
    }
}
