//! Name/id search over the item catalog (completions feed off the same port).

use std::sync::Arc;

use crate::domain::SearchHit;
use crate::error::StoreError;
use crate::ports::ItemCatalogRepository;

pub struct SearchService {
    catalog: Arc<dyn ItemCatalogRepository>,
}

impl SearchService {
    pub fn new(catalog: Arc<dyn ItemCatalogRepository>) -> Self {
        Self { catalog }
    }

    /// Prefix-first search; `limit` bounds the completion list.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, StoreError> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        self.catalog.search(query, limit.max(1))
    }
}
