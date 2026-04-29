//! Smoke tests for the generic `ThreadIndex<M>`. We use `M = Stub` (a small
//! `serde(flatten)`-friendly struct) so the test suite mirrors how
//! pi-bridge/claude-bridge will plug in their own metadata types in A2/A3.

use alleycat_bridge_core::thread_index::open_in_dir;
use alleycat_bridge_core::{
    Hydrator, IndexEntry, ListFilter, ListPage, ListSort, ThreadIndex, ThreadIndexHandle,
};
use alleycat_codex_proto::{SortDirection, ThreadSortKey, ThreadSourceKind};
use anyhow::Result;
use async_trait::async_trait;
use chrono::TimeZone;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stub {
    session_path: String,
    session_id: String,
}

fn entry(id: &str, cwd: &str, created: i64, updated: i64, archived: bool) -> IndexEntry<Stub> {
    IndexEntry {
        thread_id: id.to_string(),
        cwd: cwd.to_string(),
        created_at: created,
        updated_at: updated,
        archived,
        name: None,
        preview: format!("preview {id}"),
        forked_from_id: None,
        model_provider: "stub".to_string(),
        source: ThreadSourceKind::AppServer,
        metadata: Stub {
            session_path: format!("/sessions/{id}.jsonl"),
            session_id: format!("sid-{id}"),
        },
    }
}

#[tokio::test]
async fn open_creates_parent_directory_and_starts_empty() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("nested/codex/threads.json");
    let index: Arc<ThreadIndex<Stub>> = ThreadIndex::open_at(path.clone()).await.unwrap();
    assert!(index.snapshot().await.is_empty());
    assert!(path.parent().unwrap().is_dir());
}

#[tokio::test]
async fn insert_then_lookup_persists_round_trip() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("threads.json");
    let index: Arc<ThreadIndex<Stub>> = ThreadIndex::open_at(path.clone()).await.unwrap();

    index
        .insert(entry("a", "/work", 100, 200, false))
        .await
        .unwrap();
    let row = index.lookup("a").await.unwrap();
    assert_eq!(row.cwd, "/work");
    assert_eq!(row.metadata.session_id, "sid-a");
    assert!(path.exists());

    // Reopen from disk — the row survives, including the flattened metadata.
    drop(index);
    let reopened: Arc<ThreadIndex<Stub>> = ThreadIndex::open_at(path).await.unwrap();
    let row = reopened.lookup("a").await.unwrap();
    assert_eq!(row.metadata.session_path, "/sessions/a.jsonl");
}

#[tokio::test]
async fn metadata_flattens_into_row_json() {
    // Belt-and-braces: the on-disk JSON puts metadata fields at the row's
    // top level, not under a nested `metadata` key. pi/claude on-disk
    // compatibility relies on this.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("threads.json");
    let index: Arc<ThreadIndex<Stub>> = ThreadIndex::open_at(path.clone()).await.unwrap();
    index
        .insert(entry("a", "/work", 100, 200, false))
        .await
        .unwrap();

    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        raw.contains("\"sessionPath\""),
        "expected flattened metadata, got: {raw}"
    );
    assert!(
        raw.contains("\"sessionId\""),
        "expected flattened metadata, got: {raw}"
    );
    assert!(
        !raw.contains("\"metadata\""),
        "metadata key must not appear nested, got: {raw}"
    );
}

#[tokio::test]
async fn update_preview_and_set_archived_persist() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("threads.json");
    let index: Arc<ThreadIndex<Stub>> = ThreadIndex::open_at(path.clone()).await.unwrap();
    index
        .insert(entry("a", "/work", 100, 200, false))
        .await
        .unwrap();

    let new_ts = chrono::Utc.timestamp_millis_opt(500).unwrap();
    index
        .update_preview_and_updated_at("a", "fresh".into(), new_ts)
        .await
        .unwrap();
    let row = index.lookup("a").await.unwrap();
    assert_eq!(row.preview, "fresh");
    assert_eq!(row.updated_at, 500);

    assert!(index.set_archived("a", true).await.unwrap());
    assert!(index.lookup("a").await.unwrap().archived);
    assert!(!index.set_archived("missing", true).await.unwrap());

    // Updating a missing thread is a silent no-op.
    index
        .update_preview_and_updated_at("ghost", "ignored".into(), new_ts)
        .await
        .unwrap();
    assert!(index.lookup("ghost").await.is_none());
}

#[tokio::test]
async fn set_name_trims_blank_to_none() {
    let dir = TempDir::new().unwrap();
    let index: Arc<ThreadIndex<Stub>> = ThreadIndex::open_at(dir.path().join("t.json"))
        .await
        .unwrap();
    index.insert(entry("a", "/x", 0, 0, false)).await.unwrap();
    index.set_name("a", Some("  hello  ".into())).await.unwrap();
    assert_eq!(
        index.lookup("a").await.unwrap().name.as_deref(),
        Some("hello")
    );
    index.set_name("a", Some("   ".into())).await.unwrap();
    assert_eq!(index.lookup("a").await.unwrap().name, None);
}

#[tokio::test]
async fn list_filters_and_sorts() {
    let dir = TempDir::new().unwrap();
    let index: Arc<ThreadIndex<Stub>> = ThreadIndex::open_at(dir.path().join("t.json"))
        .await
        .unwrap();
    index
        .insert(entry("a", "/work", 100, 200, false))
        .await
        .unwrap();
    index
        .insert(entry("b", "/work", 100, 300, false))
        .await
        .unwrap();
    index
        .insert(entry("c", "/other", 100, 400, true))
        .await
        .unwrap();

    // Default: updated_at desc.
    let page = index
        .list(&ListFilter::default(), ListSort::default(), None, None)
        .await
        .unwrap();
    let ids: Vec<_> = page.data.iter().map(|t| t.thread_id.as_str()).collect();
    assert_eq!(ids, vec!["c", "b", "a"]);
    assert!(page.next_cursor.is_none());

    // Cwd filter.
    let page = index
        .list(
            &ListFilter {
                cwds: Some(vec!["/work".into()]),
                ..Default::default()
            },
            ListSort::default(),
            None,
            None,
        )
        .await
        .unwrap();
    let ids: Vec<_> = page.data.iter().map(|t| t.thread_id.as_str()).collect();
    assert_eq!(ids, vec!["b", "a"]);

    // Archived filter.
    let page = index
        .list(
            &ListFilter {
                archived: Some(true),
                ..Default::default()
            },
            ListSort::default(),
            None,
            None,
        )
        .await
        .unwrap();
    let ids: Vec<_> = page.data.iter().map(|t| t.thread_id.as_str()).collect();
    assert_eq!(ids, vec!["c"]);

    // Search filter — case-insensitive over preview/name.
    let page = index
        .list(
            &ListFilter {
                search_term: Some("PREVIEW B".into()),
                ..Default::default()
            },
            ListSort::default(),
            None,
            None,
        )
        .await
        .unwrap();
    let ids: Vec<_> = page.data.iter().map(|t| t.thread_id.as_str()).collect();
    assert_eq!(ids, vec!["b"]);

    // Created-at asc.
    let page = index
        .list(
            &ListFilter::default(),
            ListSort {
                key: ThreadSortKey::CreatedAt,
                direction: SortDirection::Asc,
            },
            None,
            None,
        )
        .await
        .unwrap();
    let ids: Vec<_> = page.data.iter().map(|t| t.thread_id.as_str()).collect();
    // All share created_at=100 — fall through to thread_id tiebreaker (asc).
    assert_eq!(ids, vec!["a", "b", "c"]);
}

#[tokio::test]
async fn list_pagination_via_cursor_walks_all() {
    let dir = TempDir::new().unwrap();
    let index: Arc<ThreadIndex<Stub>> = ThreadIndex::open_at(dir.path().join("t.json"))
        .await
        .unwrap();
    for i in 0..5 {
        index
            .insert(entry(&format!("t{i}"), "/x", 100, 1000 + i as i64, false))
            .await
            .unwrap();
    }

    let mut cursor: Option<String> = None;
    let mut seen: Vec<String> = Vec::new();
    loop {
        let page: ListPage<Stub> = index
            .list(
                &ListFilter::default(),
                ListSort::default(),
                cursor.as_deref(),
                Some(2),
            )
            .await
            .unwrap();
        assert!(page.data.len() <= 2);
        for t in &page.data {
            seen.push(t.thread_id.clone());
        }
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    assert_eq!(seen, vec!["t4", "t3", "t2", "t1", "t0"]);
}

/// Two concurrent inserts on a shared `Arc` used to race on the shared
/// `<index>.tmp` rename target — mirrors the regression test that lived in
/// pi-bridge before the lift. The persist mutex serializes them.
#[tokio::test]
async fn concurrent_inserts_do_not_race_on_temp_rename() {
    let dir = TempDir::new().unwrap();
    let index: Arc<ThreadIndex<Stub>> = ThreadIndex::open_at(dir.path().join("threads.json"))
        .await
        .unwrap();

    const N: usize = 16;
    let handles: Vec<_> = (0..N)
        .map(|i| {
            let idx = Arc::clone(&index);
            tokio::spawn(async move {
                idx.insert(entry(
                    &format!("race-{i}"),
                    "/work",
                    100,
                    200 + i as i64,
                    false,
                ))
                .await
            })
        })
        .collect();

    for (i, handle) in handles.into_iter().enumerate() {
        handle
            .await
            .expect("task did not panic")
            .unwrap_or_else(|err| panic!("insert #{i} failed: {err:#}"));
    }

    let rows = index.snapshot().await;
    assert_eq!(rows.len(), N);

    let reopened: Arc<ThreadIndex<Stub>> = ThreadIndex::open_at(dir.path().join("threads.json"))
        .await
        .unwrap();
    let on_disk = reopened.snapshot().await;
    assert_eq!(on_disk.len(), N);
}

struct StaticHydrator {
    rows: Vec<IndexEntry<Stub>>,
}

#[async_trait]
impl Hydrator<Stub> for StaticHydrator {
    async fn scan(&self) -> Result<Vec<IndexEntry<Stub>>> {
        Ok(self.rows.clone())
    }
}

#[tokio::test]
async fn open_and_hydrate_inserts_new_rows_only() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("threads.json");

    // First hydrate: index empty → all rows added.
    let hydrator = StaticHydrator {
        rows: vec![
            entry("h1", "/p", 100, 200, false),
            entry("h2", "/p", 100, 201, false),
        ],
    };
    let index: Arc<ThreadIndex<Stub>> = ThreadIndex::open_and_hydrate(path.clone(), &hydrator)
        .await
        .unwrap();
    assert_eq!(index.snapshot().await.len(), 2);

    // Second hydrate on a re-opened index with the same rows: idempotent.
    drop(index);
    let again: Arc<ThreadIndex<Stub>> = ThreadIndex::open_and_hydrate(path.clone(), &hydrator)
        .await
        .unwrap();
    assert_eq!(again.snapshot().await.len(), 2);

    // Adding a new row through hydrate_from inserts only the missing one.
    let with_extra = StaticHydrator {
        rows: vec![
            entry("h1", "/p", 100, 200, false),
            entry("h2", "/p", 100, 201, false),
            entry("h3", "/q", 100, 202, false),
        ],
    };
    let added = again.hydrate_from(&with_extra).await.unwrap();
    assert_eq!(added, 1);
    assert_eq!(again.snapshot().await.len(), 3);
}

#[tokio::test]
async fn set_forked_from_id_persists() {
    let dir = TempDir::new().unwrap();
    let index: Arc<ThreadIndex<Stub>> = ThreadIndex::open_at(dir.path().join("t.json"))
        .await
        .unwrap();
    index
        .insert(entry("child", "/p", 100, 300, false))
        .await
        .unwrap();
    assert!(
        index
            .set_forked_from_id("child", Some("parent".into()))
            .await
            .unwrap()
    );
    assert_eq!(
        index
            .lookup("child")
            .await
            .unwrap()
            .forked_from_id
            .as_deref(),
        Some("parent")
    );
    assert!(
        !index
            .set_forked_from_id("missing", Some("p".into()))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn handle_trait_object_dispatch_works() {
    // Bridges program against `Arc<dyn ThreadIndexHandle<M>>`; verify the
    // trait-object path exercises the generic implementation correctly.
    let dir = TempDir::new().unwrap();
    let real: Arc<ThreadIndex<Stub>> = ThreadIndex::open_at(dir.path().join("t.json"))
        .await
        .unwrap();
    let handle: Arc<dyn ThreadIndexHandle<Stub>> = real.clone();

    handle
        .insert(entry("a", "/x", 100, 200, false))
        .await
        .unwrap();
    let row = handle.lookup("a").await.unwrap();
    assert_eq!(row.cwd, "/x");
    assert_eq!(handle.loaded_thread_ids().await, vec!["a".to_string()]);

    let page = handle
        .list(&ListFilter::default(), ListSort::default(), None, None)
        .await
        .unwrap();
    assert_eq!(page.data.len(), 1);
}

#[tokio::test]
async fn open_in_dir_helper_round_trips() {
    // Tiny convenience: bridges typically know their codex-home dir, not the
    // full threads.json path. The helper saves the `.join("threads.json")`
    // boilerplate at every call site.
    let dir = TempDir::new().unwrap();
    let index: Arc<ThreadIndex<Stub>> = open_in_dir::<Stub>(dir.path()).await.unwrap();
    index.insert(entry("a", "/x", 1, 2, false)).await.unwrap();
    let path: PathBuf = dir.path().join("threads.json");
    assert!(path.exists());
    let _ = index.snapshot().await; // touch
}
