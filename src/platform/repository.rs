//! Novel record repository abstraction.
//!
//! The domain layer reads and writes novel records through this interface
//! instead of holding the whole database in memory (`db::DATABASE`,
//! `all_records()`). The native backend keeps the current YAML
//! database file; a Worker backend uses D1. Phase 3 migrates callers; the
//! traits and types are defined here up front.

use crate::db::NovelRecord;

/// Novel identifier (the numeric record id used across the codebase).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NovelId(pub i64);

impl From<i64> for NovelId {
    fn from(id: i64) -> Self {
        Self(id)
    }
}

impl From<NovelId> for i64 {
    fn from(id: NovelId) -> Self {
        id.0
    }
}

/// Query for listing/filtering records. Field names follow the existing
/// `Database` sort keys so native and D1 backends can share semantics.
#[derive(Debug, Clone, Default)]
pub struct NovelQuery {
    /// Free-text search over title/author/other text fields.
    pub keyword: Option<String>,
    /// Sort key from `db::sort_keys` (e.g. `general_lastup`, `title`).
    pub sort_by: Option<String>,
    pub offset: usize,
    pub limit: usize,
}

/// CRUD + query access to novel records.
///
/// Implementations decide persistence and indexing. Full-table scans are an
/// implementation detail; the D1 backend must paginate/filter at the SQL
/// level and never `SELECT` the whole table.
pub trait NovelRepository: Send + Sync {
    fn get(&self, id: NovelId) -> crate::error::Result<Option<NovelRecord>>;

    fn find_by_toc_url(&self, url: &str) -> crate::error::Result<Option<NovelRecord>>;

    fn find_by_title(&self, title: &str) -> crate::error::Result<Option<NovelRecord>>;

    fn insert(&self, record: &NovelRecord) -> crate::error::Result<()>;

    fn update(&self, record: &NovelRecord) -> crate::error::Result<()>;

    fn remove(&self, id: NovelId) -> crate::error::Result<()>;

    fn query(&self, query: &NovelQuery) -> crate::error::Result<Vec<NovelRecord>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn novel_id_conversions() {
        let id = NovelId::from(42_i64);
        assert_eq!(id.0, 42);
        let back: i64 = id.into();
        assert_eq!(back, 42);
    }
}
