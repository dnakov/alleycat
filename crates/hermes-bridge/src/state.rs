//! In-memory turn state — tracks active turns so we can correlate SSE events.

use std::collections::HashMap;
use std::sync::Mutex;

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ActiveTurn {
    pub turn_id: String,
    pub thread_id: String,
    pub hermes_session_id: String,
    pub run_id: Option<String>,
}

/// Tracks active (in-progress) turns keyed by thread id.
pub struct TurnState {
    inner: Mutex<HashMap<String, ActiveTurn>>,
}

impl TurnState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn insert(&self, thread_id: String, turn: ActiveTurn) {
        self.inner.lock().unwrap().insert(thread_id, turn);
    }

    #[allow(dead_code)]
    pub fn get(&self, thread_id: &str) -> Option<ActiveTurn> {
        self.inner.lock().unwrap().get(thread_id).cloned()
    }

    pub fn remove(&self, thread_id: &str) -> Option<ActiveTurn> {
        self.inner.lock().unwrap().remove(thread_id)
    }
}
