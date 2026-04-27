//! Integration-test mirror of `crate::index::testing::NoopThreadIndex`.
//!
//! The lib-level helper sits behind `#[cfg(any(test, feature = "test-helpers"))]`
//! which integration tests can't activate without a self-dep workaround in
//! `Cargo.toml`. This file duplicates the trivial no-op impl so verification
//! tests stay self-contained. Keep in sync with the lib version — both are
//! tiny and changes are rare.
//!
//! For tests that need full lookup/insert semantics, prefer
//! `alleycat_pi_bridge::index::ThreadIndex::open_at(tempdir)` over copying
//! `InMemoryThreadIndex` here too.

use alleycat_pi_bridge::state::{IndexEntry, ListFilter, ListPage, ListSort, ThreadIndexHandle};
use anyhow::Result;
use chrono::{DateTime, Utc};

/// Silent no-op `ThreadIndexHandle`. Construct with `Arc::new(NoopThreadIndex)`.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopThreadIndex;

#[async_trait::async_trait]
impl ThreadIndexHandle for NoopThreadIndex {
    async fn lookup(&self, _thread_id: &str) -> Option<IndexEntry> {
        None
    }
    async fn insert(&self, _entry: IndexEntry) -> Result<()> {
        Ok(())
    }
    async fn set_archived(&self, _thread_id: &str, _archived: bool) -> Result<bool> {
        Ok(false)
    }
    async fn set_name(&self, _thread_id: &str, _name: Option<String>) -> Result<bool> {
        Ok(false)
    }
    async fn update_preview_and_updated_at(
        &self,
        _thread_id: &str,
        _preview: String,
        _updated_at: DateTime<Utc>,
    ) -> Result<()> {
        Ok(())
    }
    async fn list(
        &self,
        _filter: &ListFilter,
        _sort: ListSort,
        _cursor: Option<&str>,
        _limit: Option<u32>,
    ) -> Result<ListPage> {
        Ok(ListPage {
            data: Vec::new(),
            next_cursor: None,
        })
    }
    async fn loaded_thread_ids(&self) -> Vec<String> {
        Vec::new()
    }
}
