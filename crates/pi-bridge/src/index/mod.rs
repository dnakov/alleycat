//! Bridge-side thread index.
//!
//! `ThreadIndex` is the source of truth for `thread/list`, `thread/read`, and the
//! archive flags codex maintains. It is backed by a JSON file at
//! `<codex_home>/threads.json` (atomic write-tmp-rename), with an in-memory mirror
//! protected by an async `RwLock` so handlers that only read can fan out.
//!
//! The index records one `IndexEntry` per codex thread (i.e. per pi session the
//! bridge has seen). Each entry carries enough metadata to rebuild a
//! `codex_proto::Thread` without re-reading the JSONL file: the source-of-truth
//! pi session path, cwd, name, preview, timestamps, and archive flag.
//!
//! On startup the index is hydrated by walking `~/.pi/agent/sessions/` via
//! `pi_session_scan::list_all` — any pi session not already present is inserted
//! with a fresh `thread_id`. Sessions previously seen keep their stored
//! `thread_id` and any bridge-only metadata (archived flag, custom name).

pub mod pi_session_scan;

#[cfg(any(test, feature = "test-helpers"))]
pub mod testing;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use crate::codex_proto::{
    SessionSource, SortDirection, Thread, ThreadSortKey, ThreadSourceKind, ThreadStatus,
};
use crate::state::ThreadIndexHandle;

pub use pi_session_scan::{PiSessionInfo, list_all, list_sessions_from_dir, pi_sessions_dir};

/// Bridge CLI version string baked into `Thread.cli_version`. Kept stable across
/// pi versions because codex clients use it to display origin.
pub const CLI_VERSION: &str = concat!("alleycat-pi-bridge/", env!("CARGO_PKG_VERSION"));

/// One row in the index — both the on-disk JSON shape and the in-memory cache.
/// All fields except those relating to thread bookkeeping (`archived`,
/// `forked_from_id`, `name`, `preview`) are populated from the underlying pi
/// session header at hydration time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexEntry {
    /// Codex-side thread id; stable for the life of the underlying pi session.
    pub thread_id: String,
    /// Absolute path to the pi JSONL session file.
    pub pi_session_path: PathBuf,
    /// Pi session id (the UUID inside the JSONL header).
    pub pi_session_id: String,
    /// Working directory recorded by pi at session creation.
    pub cwd: String,
    /// User-defined display name (pi `session_info` entry or `thread/name/set`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Latest first-user-message preview, refreshed when activity rolls in.
    pub preview: String,
    /// Codex `created_at` (epoch millis). Sourced from pi's session header.
    pub created_at: i64,
    /// Codex `updated_at` (epoch millis). Sourced from the latest pi message
    /// activity, or refreshed via `update_preview_and_updated_at`.
    pub updated_at: i64,
    /// Bridge-only archive flag; codex clients toggle this through
    /// `thread/archive` / `thread/unarchive`.
    #[serde(default)]
    pub archived: bool,
    /// Forked-from chain. Pi session headers carry a parent path — we resolve
    /// it lazily into a thread id when we know about both ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from_id: Option<String>,
    /// Provider tag we surface back to codex. Pi sessions don't record this on
    /// disk so we keep whatever was set at thread/start time, defaulting to
    /// `"pi"` for hydrated rows.
    pub model_provider: String,
    /// Source flavor codex displays in the UI; bridge-spawned threads are
    /// always `appServer` per the plan.
    pub source: ThreadSourceKind,
}

impl IndexEntry {
    /// Create a fresh index row from a scanned pi session, minting a new
    /// thread_id. Used during hydration.
    fn from_pi(info: &PiSessionInfo) -> Self {
        Self {
            thread_id: Uuid::now_v7().to_string(),
            pi_session_path: info.path.clone(),
            pi_session_id: info.id.clone(),
            cwd: info.cwd.clone(),
            name: info.name.clone(),
            preview: info.first_message.clone(),
            created_at: info.created.timestamp_millis(),
            updated_at: info.modified.timestamp_millis(),
            archived: false,
            forked_from_id: None,
            model_provider: "pi".to_string(),
            source: ThreadSourceKind::AppServer,
        }
    }

    /// Fold the row into a codex-shape `Thread` with no turns populated. The
    /// `thread/read` handler fills `turns` separately when `include_turns` is
    /// set.
    pub fn to_thread(&self) -> Thread {
        Thread {
            id: self.thread_id.clone(),
            forked_from_id: self.forked_from_id.clone(),
            preview: self.preview.clone(),
            ephemeral: false,
            model_provider: self.model_provider.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
            status: ThreadStatus::NotLoaded,
            path: Some(self.pi_session_path.to_string_lossy().into_owned()),
            cwd: self.cwd.clone(),
            cli_version: CLI_VERSION.to_string(),
            source: match self.source {
                ThreadSourceKind::Cli => SessionSource::Cli,
                ThreadSourceKind::VsCode => SessionSource::VsCode,
                ThreadSourceKind::Exec => SessionSource::Exec,
                ThreadSourceKind::AppServer => SessionSource::AppServer,
                _ => SessionSource::AppServer,
            },
            agent_nickname: None,
            agent_role: None,
            git_info: None,
            name: self.name.clone(),
            turns: Vec::new(),
        }
    }
}

/// Filter knobs accepted by `ThreadIndex::list`. Mirrors the subset of
/// `ThreadListParams` the bridge actually filters on.
#[derive(Debug, Default, Clone)]
pub struct ListFilter {
    /// `None` = either; `Some(true)` = only archived; `Some(false)` = only
    /// non-archived.
    pub archived: Option<bool>,
    /// One of more cwds. Matched as exact string equality against
    /// `IndexEntry.cwd`.
    pub cwds: Option<Vec<String>>,
    /// Case-insensitive substring matched against `name` and `preview`.
    pub search_term: Option<String>,
    /// Restrict to entries whose `model_provider` is in this set.
    pub model_providers: Option<Vec<String>>,
    /// Restrict to entries whose `source` is in this set.
    pub source_kinds: Option<Vec<ThreadSourceKind>>,
}

/// Sort knobs. Defaults to `updated_at desc` like codex itself.
#[derive(Debug, Clone, Copy)]
pub struct ListSort {
    pub key: ThreadSortKey,
    pub direction: SortDirection,
}

impl Default for ListSort {
    fn default() -> Self {
        Self {
            key: ThreadSortKey::UpdatedAt,
            direction: SortDirection::Desc,
        }
    }
}

/// Outcome of a paginated list call.
#[derive(Debug, Clone)]
pub struct ListPage {
    pub data: Vec<Thread>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct OnDisk {
    #[serde(default)]
    threads: Vec<IndexEntry>,
}

/// Thread-safe index handle. Cheap to clone (it is just an `Arc` internally).
pub struct ThreadIndex {
    storage_path: PathBuf,
    inner: RwLock<BTreeMap<String, IndexEntry>>,
    /// Serializes `persist()` callers. Two concurrent inserts would otherwise
    /// race on the shared `threads.json.tmp` filename: writer A creates and
    /// renames the temp, writer B writes to the now-stale path and its rename
    /// fails because the source no longer exists. The mutex turns those
    /// races into FIFO writes; readers (`list`, `lookup`, etc.) never take
    /// it. Tracked in #39.
    persist_lock: Mutex<()>,
}

impl ThreadIndex {
    /// Open the index at `<codex_home>/threads.json`, creating its parent
    /// directory if needed. Does **not** hydrate from pi — call `hydrate()` (or
    /// `open_and_hydrate()`) afterwards.
    pub async fn open(codex_home: &Path) -> Result<Arc<Self>> {
        Self::open_at(codex_home.join("threads.json")).await
    }

    /// Variant of `open` that takes the threads.json path directly. Useful for
    /// tests that want a tempdir-backed store.
    pub async fn open_at(path: PathBuf) -> Result<Arc<Self>> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating index dir {}", parent.display()))?;
        }
        let entries = match fs::read_to_string(&path).await {
            Ok(text) if !text.trim().is_empty() => {
                let parsed: OnDisk = serde_json::from_str(&text)
                    .with_context(|| format!("parsing {}", path.display()))?;
                parsed
                    .threads
                    .into_iter()
                    .map(|e| (e.thread_id.clone(), e))
                    .collect::<BTreeMap<_, _>>()
            }
            _ => BTreeMap::new(),
        };
        Ok(Arc::new(Self {
            storage_path: path,
            inner: RwLock::new(entries),
            persist_lock: Mutex::new(()),
        }))
    }

    /// Convenience: open + hydrate from the on-disk pi sessions in a single
    /// call. Equivalent to `open(codex_home) then hydrate_from_pi_dir(None)`.
    pub async fn open_and_hydrate(codex_home: &Path) -> Result<Arc<Self>> {
        let index = Self::open(codex_home).await?;
        index.hydrate_from_pi_dir(None).await?;
        Ok(index)
    }

    /// Walk a pi sessions directory (defaults to `pi_sessions_dir()`) and
    /// insert any sessions not already in the index. Returns the number of
    /// rows added.
    pub async fn hydrate_from_pi_dir(&self, override_dir: Option<&Path>) -> Result<usize> {
        let scanned = match override_dir {
            Some(dir) => {
                let mut all = Vec::new();
                let mut read_dir = match fs::read_dir(dir).await {
                    Ok(rd) => rd,
                    Err(_) => return Ok(0),
                };
                while let Ok(Some(entry)) = read_dir.next_entry().await {
                    let path = entry.path();
                    if entry
                        .file_type()
                        .await
                        .map(|ft| ft.is_dir())
                        .unwrap_or(false)
                    {
                        all.extend(list_sessions_from_dir(&path).await);
                    }
                }
                all
            }
            None => list_all().await,
        };

        let mut added = 0usize;
        let mut path_to_thread: BTreeMap<PathBuf, String> = BTreeMap::new();
        {
            let guard = self.inner.read().await;
            for entry in guard.values() {
                path_to_thread.insert(entry.pi_session_path.clone(), entry.thread_id.clone());
            }
        }

        let mut to_insert = Vec::new();
        for info in &scanned {
            if path_to_thread.contains_key(&info.path) {
                continue;
            }
            to_insert.push(IndexEntry::from_pi(info));
        }

        if !to_insert.is_empty() {
            let mut guard = self.inner.write().await;
            for entry in to_insert {
                path_to_thread.insert(entry.pi_session_path.clone(), entry.thread_id.clone());
                guard.insert(entry.thread_id.clone(), entry);
                added += 1;
            }
            drop(guard);
            self.persist().await?;
        }

        // Resolve forked_from_id for any rows whose pi parent path is now in
        // the index. Cheap pass — we touch the write lock only if anything
        // actually changed.
        let mut updates: Vec<(String, String)> = Vec::new();
        {
            let guard = self.inner.read().await;
            for entry in guard.values() {
                if entry.forked_from_id.is_some() {
                    continue;
                }
                let Some(parent_path) = scanned
                    .iter()
                    .find(|s| s.path == entry.pi_session_path)
                    .and_then(|s| s.parent_session_path.as_ref())
                else {
                    continue;
                };
                if let Some(parent_thread) = path_to_thread.get(parent_path) {
                    updates.push((entry.thread_id.clone(), parent_thread.clone()));
                }
            }
        }
        if !updates.is_empty() {
            let mut guard = self.inner.write().await;
            for (child, parent) in updates {
                if let Some(row) = guard.get_mut(&child) {
                    row.forked_from_id = Some(parent);
                }
            }
            drop(guard);
            self.persist().await?;
        }

        Ok(added)
    }

    /// Insert (or replace) an entry. Used when `thread/start`/`thread/fork`
    /// mints a new pi session.
    pub async fn insert(&self, entry: IndexEntry) -> Result<()> {
        {
            let mut guard = self.inner.write().await;
            guard.insert(entry.thread_id.clone(), entry);
        }
        self.persist().await
    }

    /// Update the rolling preview + `updated_at` (called whenever pi emits a
    /// `turn_end`/`agent_end`). Quietly ignores missing rows so callers can
    /// fire-and-forget.
    pub async fn update_preview_and_updated_at(
        &self,
        thread_id: &str,
        preview: String,
        updated_at: DateTime<Utc>,
    ) -> Result<()> {
        let changed = {
            let mut guard = self.inner.write().await;
            if let Some(row) = guard.get_mut(thread_id) {
                row.preview = preview;
                row.updated_at = updated_at.timestamp_millis();
                true
            } else {
                false
            }
        };
        if changed {
            self.persist().await?;
        }
        Ok(())
    }

    /// Toggle the archive flag. Returns `true` if the row existed.
    pub async fn set_archived(&self, thread_id: &str, archived: bool) -> Result<bool> {
        let changed = {
            let mut guard = self.inner.write().await;
            match guard.get_mut(thread_id) {
                Some(row) => {
                    row.archived = archived;
                    true
                }
                None => false,
            }
        };
        if changed {
            self.persist().await?;
        }
        Ok(changed)
    }

    /// Set the user-defined name. Pass `None` to clear it.
    pub async fn set_name(&self, thread_id: &str, name: Option<String>) -> Result<bool> {
        let changed = {
            let mut guard = self.inner.write().await;
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
                    true
                }
                None => false,
            }
        };
        if changed {
            self.persist().await?;
        }
        Ok(changed)
    }

    /// Look a row up by codex `thread_id`.
    pub async fn lookup(&self, thread_id: &str) -> Option<IndexEntry> {
        self.inner.read().await.get(thread_id).cloned()
    }

    /// Every thread id in the index, in undefined order. Handlers building
    /// `thread/loaded/list` should intersect this with `PiPool::loaded_thread_ids`
    /// — the index knows about disk sessions, the pool knows what is actually
    /// spawned right now.
    pub async fn loaded_thread_ids(&self) -> Vec<String> {
        self.inner.read().await.keys().cloned().collect()
    }

    /// Paginated, filtered, sorted listing in codex `Thread` shape.
    pub async fn list(
        &self,
        filter: &ListFilter,
        sort: ListSort,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> Result<ListPage> {
        let snapshot: Vec<IndexEntry> = self.inner.read().await.values().cloned().collect();
        let mut filtered: Vec<IndexEntry> = snapshot
            .into_iter()
            .filter(|e| matches_filter(e, filter))
            .collect();

        sort_entries(&mut filtered, sort);

        let after = cursor
            .map(decode_cursor)
            .transpose()
            .context("invalid cursor")?;
        let starting = match after {
            Some(c) => filtered
                .iter()
                .position(|e| cursor_after(e, &c, sort))
                .unwrap_or(filtered.len()),
            None => 0,
        };

        let limit = limit.map(|l| l as usize).unwrap_or(filtered.len());
        let end = (starting + limit).min(filtered.len());
        let page = &filtered[starting..end];
        let next_cursor = if end < filtered.len() {
            Some(encode_cursor(&page[page.len() - 1], sort))
        } else {
            None
        };

        Ok(ListPage {
            data: page.iter().map(IndexEntry::to_thread).collect(),
            next_cursor,
        })
    }

    /// Snapshot the entire index. Test helper.
    #[cfg(test)]
    async fn all(&self) -> Vec<IndexEntry> {
        self.inner.read().await.values().cloned().collect()
    }

    async fn persist(&self) -> Result<()> {
        // FIFO writers. Two concurrent inserts would otherwise race on the
        // shared `threads.json.tmp` filename — see `persist_lock` doc on the
        // struct. Snapshot the in-memory map *inside* the critical section so
        // the on-disk file always reflects the latest committed state at
        // rename time.
        let _guard = self.persist_lock.lock().await;
        let snapshot: Vec<IndexEntry> = self.inner.read().await.values().cloned().collect();
        let payload = OnDisk { threads: snapshot };
        let serialized = serde_json::to_vec_pretty(&payload).context("serializing threads.json")?;
        let tmp = self.storage_path.with_extension("json.tmp");
        fs::write(&tmp, &serialized)
            .await
            .with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &self.storage_path)
            .await
            .with_context(|| {
                format!(
                    "renaming {} -> {}",
                    tmp.display(),
                    self.storage_path.display()
                )
            })?;
        Ok(())
    }
}

/// Bridge `ThreadIndex`'s inherent methods to the handler-facing trait.
/// Owned `String`/`IndexEntry` arguments mirror the choice on `PiPoolHandle`:
/// `async_trait` + object safety prefer non-borrowed parameters.
#[async_trait::async_trait]
impl ThreadIndexHandle for ThreadIndex {
    async fn lookup(&self, thread_id: &str) -> Option<IndexEntry> {
        ThreadIndex::lookup(self, thread_id).await
    }

    async fn insert(&self, entry: IndexEntry) -> Result<()> {
        ThreadIndex::insert(self, entry).await
    }

    async fn set_archived(&self, thread_id: &str, archived: bool) -> Result<bool> {
        ThreadIndex::set_archived(self, thread_id, archived).await
    }

    async fn set_name(&self, thread_id: &str, name: Option<String>) -> Result<bool> {
        ThreadIndex::set_name(self, thread_id, name).await
    }

    async fn update_preview_and_updated_at(
        &self,
        thread_id: &str,
        preview: String,
        updated_at: DateTime<Utc>,
    ) -> Result<()> {
        ThreadIndex::update_preview_and_updated_at(self, thread_id, preview, updated_at).await
    }

    async fn list(
        &self,
        filter: &ListFilter,
        sort: ListSort,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> Result<ListPage> {
        ThreadIndex::list(self, filter, sort, cursor, limit).await
    }

    async fn loaded_thread_ids(&self) -> Vec<String> {
        ThreadIndex::loaded_thread_ids(self).await
    }
}

fn matches_filter(entry: &IndexEntry, filter: &ListFilter) -> bool {
    if let Some(want) = filter.archived {
        if entry.archived != want {
            return false;
        }
    }
    if let Some(cwds) = &filter.cwds {
        if !cwds.iter().any(|c| c == &entry.cwd) {
            return false;
        }
    }
    if let Some(providers) = &filter.model_providers {
        if !providers.iter().any(|p| p == &entry.model_provider) {
            return false;
        }
    }
    if let Some(sources) = &filter.source_kinds {
        if !sources.iter().any(|s| *s == entry.source) {
            return false;
        }
    }
    if let Some(term) = filter.search_term.as_deref().filter(|t| !t.is_empty()) {
        let needle = term.to_lowercase();
        let in_name = entry
            .name
            .as_deref()
            .map(|n| n.to_lowercase().contains(&needle))
            .unwrap_or(false);
        let in_preview = entry.preview.to_lowercase().contains(&needle);
        if !in_name && !in_preview {
            return false;
        }
    }
    true
}

fn sort_entries(entries: &mut [IndexEntry], sort: ListSort) {
    entries.sort_by(|a, b| {
        let (ak, bk) = match sort.key {
            ThreadSortKey::CreatedAt => (a.created_at, b.created_at),
            ThreadSortKey::UpdatedAt => (a.updated_at, b.updated_at),
        };
        let primary = ak.cmp(&bk);
        let primary = if matches!(sort.direction, SortDirection::Desc) {
            primary.reverse()
        } else {
            primary
        };
        // Tiebreaker on thread_id keeps pagination deterministic across
        // entries whose timestamps collide.
        primary.then_with(|| a.thread_id.cmp(&b.thread_id))
    });
}

#[derive(Debug, Serialize, Deserialize)]
struct CursorPayload {
    /// `created_at` or `updated_at` depending on the sort key.
    ts: i64,
    id: String,
}

fn encode_cursor(entry: &IndexEntry, sort: ListSort) -> String {
    let ts = match sort.key {
        ThreadSortKey::CreatedAt => entry.created_at,
        ThreadSortKey::UpdatedAt => entry.updated_at,
    };
    let payload = CursorPayload {
        ts,
        id: entry.thread_id.clone(),
    };
    let json = serde_json::to_vec(&payload).expect("CursorPayload always serializes");
    URL_SAFE_NO_PAD.encode(json)
}

fn decode_cursor(raw: &str) -> Result<CursorPayload> {
    let bytes = URL_SAFE_NO_PAD
        .decode(raw)
        .context("base64-decoding cursor")?;
    let payload: CursorPayload =
        serde_json::from_slice(&bytes).context("parsing cursor payload")?;
    Ok(payload)
}

/// True if `entry` should appear *after* the cursor under the configured sort.
/// Used to find the slice start when paginating.
fn cursor_after(entry: &IndexEntry, cursor: &CursorPayload, sort: ListSort) -> bool {
    let entry_ts = match sort.key {
        ThreadSortKey::CreatedAt => entry.created_at,
        ThreadSortKey::UpdatedAt => entry.updated_at,
    };
    let primary = entry_ts.cmp(&cursor.ts);
    let primary = if matches!(sort.direction, SortDirection::Desc) {
        primary.reverse()
    } else {
        primary
    };
    match primary {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => entry.thread_id.cmp(&cursor.id) == std::cmp::Ordering::Greater,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::io::Write;
    use tempfile::TempDir;

    fn entry(id: &str, cwd: &str, created: i64, updated: i64, archived: bool) -> IndexEntry {
        IndexEntry {
            thread_id: id.to_string(),
            pi_session_path: PathBuf::from(format!("/sessions/{id}.jsonl")),
            pi_session_id: format!("pi-{id}"),
            cwd: cwd.to_string(),
            name: None,
            preview: format!("preview {id}"),
            created_at: created,
            updated_at: updated,
            archived,
            forked_from_id: None,
            model_provider: "pi".to_string(),
            source: ThreadSourceKind::AppServer,
        }
    }

    #[tokio::test]
    async fn open_creates_parent_directory_and_starts_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested/codex/threads.json");
        let index = ThreadIndex::open_at(path.clone()).await.unwrap();
        assert!(index.all().await.is_empty());
        assert!(path.parent().unwrap().is_dir());
    }

    #[tokio::test]
    async fn insert_then_lookup_roundtrips() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("threads.json");
        let index = ThreadIndex::open_at(path.clone()).await.unwrap();

        index
            .insert(entry("a", "/work", 100, 200, false))
            .await
            .unwrap();
        let row = index.lookup("a").await.unwrap();
        assert_eq!(row.cwd, "/work");
        assert!(path.exists());

        // Re-open from disk, the row survives.
        drop(index);
        let reopened = ThreadIndex::open_at(path).await.unwrap();
        assert_eq!(reopened.lookup("a").await.unwrap().pi_session_id, "pi-a");
    }

    #[tokio::test]
    async fn update_preview_and_set_archived_persist() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("threads.json");
        let index = ThreadIndex::open_at(path.clone()).await.unwrap();
        index
            .insert(entry("a", "/work", 100, 200, false))
            .await
            .unwrap();

        let new_ts = Utc.timestamp_millis_opt(500).unwrap();
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
        let index = ThreadIndex::open_at(dir.path().join("t.json"))
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
        let index = ThreadIndex::open_at(dir.path().join("t.json"))
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
        let ids: Vec<_> = page.data.iter().map(|t| t.id.as_str()).collect();
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
        let ids: Vec<_> = page.data.iter().map(|t| t.id.as_str()).collect();
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
        let ids: Vec<_> = page.data.iter().map(|t| t.id.as_str()).collect();
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
        let ids: Vec<_> = page.data.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["b"]);
    }

    #[tokio::test]
    async fn list_pagination_via_cursor_walks_all() {
        let dir = TempDir::new().unwrap();
        let index = ThreadIndex::open_at(dir.path().join("t.json"))
            .await
            .unwrap();
        for i in 0..5 {
            // Distinct updated_at values so order is stable.
            index
                .insert(entry(&format!("t{i}"), "/x", 100, 1000 + i as i64, false))
                .await
                .unwrap();
        }

        let mut cursor: Option<String> = None;
        let mut seen: Vec<String> = Vec::new();
        loop {
            let page = index
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
                seen.push(t.id.clone());
            }
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        // Sorted by updated_at desc — newest first.
        assert_eq!(seen, vec!["t4", "t3", "t2", "t1", "t0"]);
    }

    /// Regression for #39: two concurrent `insert` calls on a shared `Arc`
    /// of the same index used to race on the shared `threads.json.tmp`
    /// rename target — one writer would create+rename, the other would
    /// then try to rename a temp file that no longer existed and fail with
    /// `renaming threads.json.tmp -> threads.json`. Fixed by serializing
    /// `persist()` behind `persist_lock`. This test fans out N inserts and
    /// asserts every one succeeds AND every row is in the final snapshot.
    #[tokio::test]
    async fn concurrent_inserts_do_not_race_on_temp_rename() {
        let dir = TempDir::new().unwrap();
        let index = ThreadIndex::open_at(dir.path().join("threads.json"))
            .await
            .unwrap();

        const N: usize = 16;
        let handles: Vec<_> = (0..N)
            .map(|i| {
                let idx = std::sync::Arc::clone(&index);
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

        // Every row landed in the index.
        let rows = index.all().await;
        assert_eq!(rows.len(), N, "all inserts should be present in memory");
        let mut ids: Vec<_> = rows.iter().map(|e| e.thread_id.clone()).collect();
        ids.sort();
        let mut want: Vec<_> = (0..N).map(|i| format!("race-{i}")).collect();
        want.sort();
        assert_eq!(ids, want);

        // And the on-disk file reflects the final state — re-open and check.
        let reopened = ThreadIndex::open_at(dir.path().join("threads.json"))
            .await
            .unwrap();
        let on_disk = reopened.all().await;
        assert_eq!(on_disk.len(), N, "on-disk threads.json must hold every row");
    }

    #[tokio::test]
    async fn hydrate_from_pi_dir_inserts_new_sessions_only() {
        let dir = TempDir::new().unwrap();
        let pi_root = dir.path().join("sessions");
        let cwd_dir = pi_root.join("encoded-cwd");
        std::fs::create_dir_all(&cwd_dir).unwrap();
        let session_path = cwd_dir.join("s1.jsonl");
        let mut f = std::fs::File::create(&session_path).unwrap();
        writeln!(
            f,
            "{}",
            r#"{"type":"session","version":3,"id":"pi-1","timestamp":"2026-04-27T10:00:00Z","cwd":"/proj"}"#
        )
        .unwrap();
        writeln!(
            f,
            "{}",
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-04-27T10:00:05Z","message":{"role":"user","content":"hi"}}"#
        )
        .unwrap();
        drop(f);

        let index = ThreadIndex::open_at(dir.path().join("threads.json"))
            .await
            .unwrap();
        let added = index.hydrate_from_pi_dir(Some(&pi_root)).await.unwrap();
        assert_eq!(added, 1);

        let rows = index.all().await;
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.pi_session_id, "pi-1");
        assert_eq!(row.cwd, "/proj");
        assert_eq!(row.preview, "hi");

        // Re-hydrating doesn't dupe.
        let added_again = index.hydrate_from_pi_dir(Some(&pi_root)).await.unwrap();
        assert_eq!(added_again, 0);
        assert_eq!(index.all().await.len(), 1);
    }

    #[tokio::test]
    async fn hydrate_resolves_forked_from_id_when_parent_in_index() {
        let dir = TempDir::new().unwrap();
        let pi_root = dir.path().join("sessions");
        let cwd_dir = pi_root.join("encoded");
        std::fs::create_dir_all(&cwd_dir).unwrap();

        let parent_path = cwd_dir.join("parent.jsonl");
        std::fs::write(
            &parent_path,
            r#"{"type":"session","version":3,"id":"parent","timestamp":"2026-04-27T10:00:00Z","cwd":"/p"}
"#,
        )
        .unwrap();

        let parent_path_str = parent_path.to_string_lossy().into_owned();
        let child_header = format!(
            r#"{{"type":"session","version":3,"id":"child","timestamp":"2026-04-27T11:00:00Z","cwd":"/p","parentSession":"{parent_path_str}"}}
"#
        );
        std::fs::write(cwd_dir.join("child.jsonl"), child_header).unwrap();

        let index = ThreadIndex::open_at(dir.path().join("threads.json"))
            .await
            .unwrap();
        index.hydrate_from_pi_dir(Some(&pi_root)).await.unwrap();

        let rows = index.all().await;
        let parent_row = rows
            .iter()
            .find(|r| r.pi_session_id == "parent")
            .expect("parent");
        let child_row = rows
            .iter()
            .find(|r| r.pi_session_id == "child")
            .expect("child");
        assert_eq!(
            child_row.forked_from_id.as_deref(),
            Some(parent_row.thread_id.as_str())
        );
    }

    #[tokio::test]
    async fn trait_object_is_usable_via_arc_dyn_handle() {
        // The whole point of the trait: handlers can program against
        // `Arc<dyn ThreadIndexHandle>` without caring whether the real impl is
        // disk-backed or in-memory. The in-memory stub itself lives in
        // `crate::index::testing` so handler crates can reuse it.
        let stub: std::sync::Arc<dyn crate::state::ThreadIndexHandle> =
            std::sync::Arc::new(super::testing::InMemoryThreadIndex::new());

        let mut row = entry("a", "/work", 100, 200, false);
        row.preview = "initial".to_string();
        stub.insert(row.clone()).await.unwrap();

        let fetched = stub.lookup("a").await.expect("row present");
        assert_eq!(fetched.preview, "initial");

        let ts = chrono::Utc.timestamp_millis_opt(300).unwrap();
        stub.update_preview_and_updated_at("a", "fresh".into(), ts)
            .await
            .unwrap();
        assert_eq!(stub.lookup("a").await.unwrap().preview, "fresh");

        assert!(stub.set_archived("a", true).await.unwrap());
        assert!(!stub.set_archived("missing", true).await.unwrap());

        let page = stub
            .list(&ListFilter::default(), ListSort::default(), None, None)
            .await
            .unwrap();
        assert_eq!(page.data.len(), 1);
        assert_eq!(stub.loaded_thread_ids().await, vec!["a".to_string()]);
    }

    #[tokio::test]
    async fn real_thread_index_is_usable_via_arc_dyn_handle() {
        // Same test but against the production type, to make sure the trait
        // object dispatch works with real disk persistence too.
        let dir = TempDir::new().unwrap();
        let real = ThreadIndex::open_at(dir.path().join("t.json"))
            .await
            .unwrap();
        let handle: std::sync::Arc<dyn crate::state::ThreadIndexHandle> = real.clone();

        handle
            .insert(entry("a", "/x", 100, 200, false))
            .await
            .unwrap();
        let row = handle.lookup("a").await.unwrap();
        assert_eq!(row.cwd, "/x");
        assert_eq!(handle.loaded_thread_ids().await, vec!["a".to_string()]);
    }
}
