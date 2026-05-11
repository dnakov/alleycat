//! Thread index — maps codex thread IDs to Hermes session IDs.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// A single binding: codex thread ↔ Hermes session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HermesBinding {
    pub thread_id: String,
    pub hermes_session_id: String,
    pub model: Option<String>,
    pub created_at: i64,
    pub preview: Option<String>,
}

/// Persisted index structure.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedIndex {
    bindings: Vec<HermesBinding>,
}

/// Thread-safe in-memory (optionally persisted) thread index.
pub struct ThreadIndex {
    path: PathBuf,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    by_thread: HashMap<String, HermesBinding>,
    by_session: HashMap<String, String>,
}

impl ThreadIndex {
    /// Create an in-memory index (no persistence).
    pub fn new_in_memory() -> Self {
        Self {
            path: PathBuf::from("/dev/null"),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Open or create the index at `path`.
    #[allow(dead_code)]
    pub async fn open(path: PathBuf) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let persisted = match tokio::fs::read_to_string(&path).await {
            Ok(text) if !text.trim().is_empty() => serde_json::from_str::<PersistedIndex>(&text)?,
            _ => PersistedIndex::default(),
        };
        let mut by_thread = HashMap::new();
        let mut by_session = HashMap::new();
        for b in persisted.bindings {
            let sid = b.hermes_session_id.clone();
            let tid = b.thread_id.clone();
            by_session.insert(sid, tid.clone());
            by_thread.insert(tid, b);
        }
        Ok(Self {
            path,
            inner: Mutex::new(Inner {
                by_thread,
                by_session,
            }),
        })
    }

    /// Insert or update a binding.
    pub fn upsert(&self, binding: HermesBinding) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(old) = inner.by_thread.remove(&binding.thread_id) {
            inner.by_session.remove(&old.hermes_session_id);
        }
        let sid = binding.hermes_session_id.clone();
        let tid = binding.thread_id.clone();
        inner.by_session.insert(sid, tid.clone());
        inner.by_thread.insert(tid, binding);
    }

    /// Look up a binding by codex thread id.
    pub fn get_by_thread(&self, thread_id: &str) -> Option<HermesBinding> {
        self.inner.lock().unwrap().by_thread.get(thread_id).cloned()
    }

    /// Look up a codex thread id by Hermes session id.
    #[allow(dead_code)]
    pub fn get_thread_id_by_session(&self, session_id: &str) -> Option<String> {
        self.inner
            .lock()
            .unwrap()
            .by_session
            .get(session_id)
            .cloned()
    }

    /// Remove a binding and return it.
    pub fn remove(&self, thread_id: &str) -> Option<HermesBinding> {
        let mut inner = self.inner.lock().unwrap();
        let binding = inner.by_thread.remove(thread_id);
        if let Some(ref b) = binding {
            inner.by_session.remove(&b.hermes_session_id);
        }
        binding
    }

    /// All thread ids in the index.
    pub fn thread_ids(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .by_thread
            .keys()
            .cloned()
            .collect()
    }

    /// Persist to disk.
    pub fn persist(&self) -> anyhow::Result<()> {
        let inner = self.inner.lock().unwrap();
        let bindings: Vec<_> = inner.by_thread.values().cloned().collect();
        let data = PersistedIndex { bindings };
        let json = serde_json::to_string_pretty(&data)?;
        std::fs::write(&self.path, json)?;
        Ok(())
    }
}
