//! Test-only `ThreadIndexHandle` implementations.
//!
//! Two flavors are exposed:
//!
//! - [`NoopThreadIndex`] — every method is a silent no-op. `lookup` always
//!   misses, mutations succeed without storing anything, `list` returns an
//!   empty page. Use when the handler under test never actually reads the
//!   index (e.g. tests for `command/exec` or `model/list` that only need a
//!   `ConnectionState` to exist).
//!
//! - [`InMemoryThreadIndex`] — a `HashMap`-backed impl with full round-trip
//!   semantics. Use when the test needs to insert rows up front and assert on
//!   `lookup` / `list` after the handler runs.
//!
//! Both are gated behind `cfg(any(test, feature = "test-helpers"))` so they
//! never ship in release binaries. Handler test mods import them via
//! `use crate::index::testing::NoopThreadIndex;`.

use std::collections::HashMap;

use anyhow::Result;
use chrono::{DateTime, Utc};
use tokio::sync::Mutex;

use crate::state::{IndexEntry, ListFilter, ListPage, ListSort, ThreadIndexHandle};

/// `ThreadIndexHandle` impl whose methods all do nothing. Construct with
/// `Arc::new(NoopThreadIndex)` and pass to `ConnectionState::new`. Suitable
/// for tests that only need the bridge state to exist; the handler under test
/// must not depend on any persisted index rows.
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

/// `ThreadIndexHandle` impl backed by a `HashMap`, with the same lookup /
/// mutation semantics as the disk-backed `ThreadIndex`. `list` ignores
/// pagination, sort, and filter knobs — handler tests that need those should
/// use the real `ThreadIndex` against a tempdir instead.
#[derive(Debug, Default)]
pub struct InMemoryThreadIndex {
    rows: Mutex<HashMap<String, IndexEntry>>,
}

impl InMemoryThreadIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Synchronous variant of `insert` for setup-time row population. Locks
    /// the mutex internally; do not hold the result across an `.await`.
    pub async fn seed(&self, entry: IndexEntry) {
        self.rows
            .lock()
            .await
            .insert(entry.thread_id.clone(), entry);
    }
}

#[async_trait::async_trait]
impl ThreadIndexHandle for InMemoryThreadIndex {
    async fn lookup(&self, thread_id: &str) -> Option<IndexEntry> {
        self.rows.lock().await.get(thread_id).cloned()
    }
    async fn insert(&self, entry: IndexEntry) -> Result<()> {
        self.rows
            .lock()
            .await
            .insert(entry.thread_id.clone(), entry);
        Ok(())
    }
    async fn set_archived(&self, thread_id: &str, archived: bool) -> Result<bool> {
        let mut guard = self.rows.lock().await;
        match guard.get_mut(thread_id) {
            Some(row) => {
                row.archived = archived;
                Ok(true)
            }
            None => Ok(false),
        }
    }
    async fn set_name(&self, thread_id: &str, name: Option<String>) -> Result<bool> {
        let mut guard = self.rows.lock().await;
        match guard.get_mut(thread_id) {
            Some(row) => {
                row.name = name.and_then(|n| {
                    let trimmed = n.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    }
                });
                Ok(true)
            }
            None => Ok(false),
        }
    }
    async fn update_preview_and_updated_at(
        &self,
        thread_id: &str,
        preview: String,
        updated_at: DateTime<Utc>,
    ) -> Result<()> {
        if let Some(row) = self.rows.lock().await.get_mut(thread_id) {
            row.preview = preview;
            row.updated_at = updated_at.timestamp_millis();
        }
        Ok(())
    }
    async fn list(
        &self,
        _filter: &ListFilter,
        _sort: ListSort,
        _cursor: Option<&str>,
        _limit: Option<u32>,
    ) -> Result<ListPage> {
        let data = self
            .rows
            .lock()
            .await
            .values()
            .map(IndexEntry::to_thread)
            .collect();
        Ok(ListPage {
            data,
            next_cursor: None,
        })
    }
    async fn loaded_thread_ids(&self) -> Vec<String> {
        self.rows.lock().await.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_proto::ThreadSourceKind;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn sample_entry(id: &str) -> IndexEntry {
        IndexEntry {
            thread_id: id.to_string(),
            pi_session_path: PathBuf::from(format!("/sessions/{id}.jsonl")),
            pi_session_id: format!("pi-{id}"),
            cwd: "/work".to_string(),
            name: None,
            preview: format!("preview {id}"),
            created_at: 100,
            updated_at: 200,
            archived: false,
            forked_from_id: None,
            model_provider: "pi".to_string(),
            source: ThreadSourceKind::AppServer,
        }
    }

    #[tokio::test]
    async fn noop_lookup_misses_and_mutations_succeed() {
        let stub: Arc<dyn ThreadIndexHandle> = Arc::new(NoopThreadIndex);
        assert!(stub.lookup("anything").await.is_none());
        stub.insert(sample_entry("a")).await.unwrap();
        assert!(stub.lookup("a").await.is_none(), "noop never stores");
        assert!(!stub.set_archived("a", true).await.unwrap());
        let page = stub
            .list(&ListFilter::default(), ListSort::default(), None, None)
            .await
            .unwrap();
        assert!(page.data.is_empty());
        assert!(stub.loaded_thread_ids().await.is_empty());
    }

    #[tokio::test]
    async fn in_memory_round_trips_an_index_entry() {
        // The exact "trivial sanity test" called out in #23.
        let stub: Arc<dyn ThreadIndexHandle> = Arc::new(InMemoryThreadIndex::new());
        let entry = sample_entry("a");
        stub.insert(entry.clone()).await.unwrap();
        let fetched = stub.lookup("a").await.expect("row stored");
        assert_eq!(fetched, entry);
        assert_eq!(stub.loaded_thread_ids().await, vec!["a".to_string()]);
    }

    #[tokio::test]
    async fn in_memory_set_archived_and_set_name_persist() {
        let stub = InMemoryThreadIndex::new();
        stub.seed(sample_entry("a")).await;
        assert!(stub.set_archived("a", true).await.unwrap());
        assert!(stub.lookup("a").await.unwrap().archived);
        assert!(stub.set_name("a", Some("  hi  ".into())).await.unwrap());
        assert_eq!(stub.lookup("a").await.unwrap().name.as_deref(), Some("hi"));
        assert!(stub.set_name("a", Some("   ".into())).await.unwrap());
        assert_eq!(stub.lookup("a").await.unwrap().name, None);
    }
}
