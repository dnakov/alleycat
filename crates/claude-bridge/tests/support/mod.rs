//! Shared helpers for the claude-bridge end-to-end tests. Mirrors the
//! pi-bridge `tests/support/mod.rs` shape so familiar patterns transfer.
//!
//! - [`fake_claude_path`] returns the path to the `fake-claude` binary cargo
//!   built for us. Tests pass it to `ClaudePool::new` (or set
//!   `CLAUDE_BRIDGE_CLAUDE_BIN` when launching the bridge binary).
//! - [`alleycat_claude_bridge_path`] is the bridge binary, used by the
//!   subprocess smoke driver in `smoke_binary.rs`.
//! - [`write_script`] persists a list of `serde_json::Value` events to a
//!   `FAKE_CLAUDE_SCRIPT` file the fake replays per turn.
//! - [`NoopThreadIndex`] satisfies `ThreadIndexHandle` for tests that
//!   don't care about persistence.
//!
//! ## Footgun: cargo's in-binary parallelism vs shared env-var fixtures
//!
//! Same caveat that pi-bridge documents: each `tests/<name>.rs` file is its
//! own integration binary, so two `#[tokio::test]` fns inside one binary
//! that both touch `FAKE_CLAUDE_SCRIPT` / `CLAUDE_BRIDGE_CLAUDE_BIN` will
//! race. Either keep one scenario per file or guard with a `Mutex`.

#![allow(dead_code)]

use std::io::Write;
use std::path::{Path, PathBuf};

use alleycat_bridge_core::ThreadIndexHandle;
use alleycat_claude_bridge::index::{ClaudeSessionRef, IndexEntry, ListFilter, ListPage, ListSort};
use async_trait::async_trait;
use serde_json::Value;
use tempfile::TempDir;

/// Path to the test-only `fake-claude` binary cargo built alongside the
/// integration tests. The `CARGO_BIN_EXE_<name>` env var is set by cargo
/// for every `[[bin]]` declared in the same crate.
pub fn fake_claude_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake-claude"))
}

/// Path to the `alleycat-claude-bridge` binary cargo built alongside the
/// integration tests. Used by the subprocess smoke driver.
pub fn alleycat_claude_bridge_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_alleycat-claude-bridge"))
}

/// Persist `events` as a JSONL file the fake-claude binary can load via
/// the `FAKE_CLAUDE_SCRIPT` env var. Returns the path so the caller can
/// set the env var (or pass it to a child process).
pub fn write_script(dir: &Path, events: &[Value]) -> PathBuf {
    let path = dir.join("script.jsonl");
    let mut f = std::fs::File::create(&path).expect("create script");
    for event in events {
        let line = serde_json::to_string(event).expect("serialize event");
        writeln!(f, "{line}").expect("write event");
    }
    path
}

/// Tempdir-backed `~/.claude` stand-in for tests that care about the
/// on-disk session scan. Drop guard restores the previous env.
pub struct ClaudeHomeFixture {
    dir: TempDir,
    prev: Option<String>,
}

impl ClaudeHomeFixture {
    pub fn new() -> Self {
        let dir = TempDir::new().expect("tempdir");
        let prev = std::env::var("CLAUDE_HOME").ok();
        unsafe {
            std::env::set_var("CLAUDE_HOME", dir.path().as_os_str());
        }
        Self { dir, prev }
    }

    pub fn home_dir(&self) -> &Path {
        self.dir.path()
    }
}

impl Drop for ClaudeHomeFixture {
    fn drop(&mut self) {
        unsafe {
            match self.prev.take() {
                Some(v) => std::env::set_var("CLAUDE_HOME", v),
                None => std::env::remove_var("CLAUDE_HOME"),
            }
        }
    }
}

/// `ThreadIndexHandle` impl that returns empty / no-op for every method.
/// Used by smoke tests that don't exercise the index.
pub struct NoopThreadIndex;

#[async_trait]
impl ThreadIndexHandle<ClaudeSessionRef> for NoopThreadIndex {
    async fn lookup(&self, _thread_id: &str) -> Option<IndexEntry> {
        None
    }

    async fn insert(&self, _entry: IndexEntry) -> anyhow::Result<()> {
        Ok(())
    }

    async fn set_archived(&self, _thread_id: &str, _archived: bool) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn set_name(&self, _thread_id: &str, _name: Option<String>) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn update_preview_and_updated_at(
        &self,
        _thread_id: &str,
        _preview: String,
        _updated_at: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn list(
        &self,
        _filter: &ListFilter,
        _sort: ListSort,
        _cursor: Option<&str>,
        _limit: Option<u32>,
    ) -> anyhow::Result<ListPage<ClaudeSessionRef>> {
        Ok(ListPage::<ClaudeSessionRef> {
            data: Vec::new(),
            next_cursor: None,
        })
    }

    async fn loaded_thread_ids(&self) -> Vec<String> {
        Vec::new()
    }
}
