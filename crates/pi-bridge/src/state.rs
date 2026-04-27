//! Per-connection state.
//!
//! One `ConnectionState` exists per connected codex client (today: one stdio
//! pair, but the design allows for multiplexed transports later). Handlers
//! borrow it through `Arc<ConnectionState>`; mutable bits live behind their
//! own locks rather than wrapping the whole struct in a `Mutex`, so a
//! long-running turn does not block unrelated requests.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use crate::codex_proto::{
    ApprovalsReviewer, AskForApproval, InitializeCapabilities, JsonRpcMessage, ReasoningEffort,
    RequestId, SandboxMode,
};
use crate::pool::PiPool;

/// Per-connection bridge state. Cheap to clone: every field is either copy
/// (small enums, atomics in the future) or `Arc`/`Mutex`-protected.
pub struct ConnectionState {
    /// Negotiated capabilities. Set once on `initialize`; subsequent reads
    /// are lock-free via `ArcSwap` semantics simulated through `Mutex` for
    /// simplicity. Replaced wholesale on the rare reconnect-style flow.
    capabilities: Mutex<Capabilities>,

    /// Default config the bridge applies to new threads. Mirrors the keys
    /// codex's `ThreadStartParams` accepts but with bridge defaults filled in.
    /// Mutable so `config/value/write` and `config/batchWrite` can update it.
    defaults: Mutex<ThreadDefaults>,

    /// Outbound notification sink — anything written here goes back to the
    /// codex client as a JSON-RPC frame. Set up by main.rs once the writer
    /// task is running.
    notification_tx: mpsc::UnboundedSender<JsonRpcMessage>,

    /// In-flight server→client requests we are waiting on. Keyed by the
    /// request id we sent to the client; the oneshot resolves with the
    /// raw JSON `result` body so handlers can deserialize into the specific
    /// response type they expect.
    pending_server_requests: AsyncMutex<HashMap<RequestId, PendingServerRequest>>,

    /// Pi process pool. Concrete `Arc<PiPool>` rather than a trait object —
    /// pi-runtime and handlers are tightly coupled within the same crate, and
    /// the index team's `fake-pi` subprocess harness already gives tests a
    /// real-process seam (better than a mock trait), so the trait abstraction
    /// added churn without a real seam to defend.
    pi_pool: Arc<PiPool>,

    /// Thread index handle (threads.json). Implemented by `crate::index::*`.
    thread_index: Arc<dyn ThreadIndexHandle>,
}

/// Negotiated client capabilities. Defaults to "no opt-outs, no experimental
/// API" so handlers can call `should_emit` even before `initialize` lands.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    pub experimental_api: bool,
    /// Methods the client asked us to suppress for this connection. Stored as
    /// a `HashSet` for O(1) `should_emit` checks.
    pub opt_out_notification_methods: HashSet<String>,
    /// Latest `clientInfo.name`/`title`/`version` triple, useful for logs.
    pub client_name: Option<String>,
    pub client_title: Option<String>,
    pub client_version: Option<String>,
}

/// Bridge defaults for a new thread. These are seeded on construction and
/// can be overridden per-`thread/start` request via `ThreadStartParams`.
#[derive(Debug, Clone, Default)]
pub struct ThreadDefaults {
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub approval_policy: Option<AskForApproval>,
    pub approvals_reviewer: Option<ApprovalsReviewer>,
    pub sandbox: Option<SandboxMode>,
    /// `service_name` from `thread/start.serviceName`. Persisted so the
    /// bridge can name the underlying pi session consistently.
    pub service_name: Option<String>,
}

/// Slot for one in-flight server→client request awaiting the client's reply.
pub struct PendingServerRequest {
    /// JSON-RPC method that was sent (for logs and timeout messages).
    pub method: String,
    /// Resolved with the `result` JSON value the client returned.
    pub responder: oneshot::Sender<Result<serde_json::Value, ServerRequestError>>,
}

#[derive(Debug, Clone)]
pub enum ServerRequestError {
    /// Client reported a JSON-RPC error.
    Rpc { code: i64, message: String },
    /// The connection closed before the client answered.
    ConnectionClosed,
    /// Local timeout fired before the client answered.
    TimedOut,
}

// === index handle trait ====================================================
//
// `state.rs` still uses a trait object for the thread index because the index
// crate evolves independently and is touched by fewer call sites; the small
// abstraction cost is worth it. The pi pool went the other way — see
// `pi_pool` above.

pub use crate::index::{IndexEntry, ListFilter, ListPage, ListSort};

/// Handler-facing surface of the thread index. The concrete implementation
/// lives in [`crate::index::ThreadIndex`]; handlers and tests program against
/// `Arc<dyn ThreadIndexHandle>` so an in-memory stub can stand in for the
/// disk-backed store.
///
/// Note: `loaded_thread_ids` returns *every* thread id the index knows about
/// (i.e. every pi session it has ingested). The `thread/loaded/list` codex
/// method is about pi processes the bridge has actively spawned — that
/// information lives on the pi pool. Handlers should intersect the two when
/// producing the codex response.
#[async_trait::async_trait]
pub trait ThreadIndexHandle: Send + Sync + 'static {
    async fn lookup(&self, thread_id: &str) -> Option<IndexEntry>;
    async fn insert(&self, entry: IndexEntry) -> anyhow::Result<()>;
    async fn set_archived(&self, thread_id: &str, archived: bool) -> anyhow::Result<bool>;
    async fn set_name(&self, thread_id: &str, name: Option<String>) -> anyhow::Result<bool>;
    async fn update_preview_and_updated_at(
        &self,
        thread_id: &str,
        preview: String,
        updated_at: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<()>;
    async fn list(
        &self,
        filter: &ListFilter,
        sort: ListSort,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> anyhow::Result<ListPage>;
    async fn loaded_thread_ids(&self) -> Vec<String>;
}

impl ConnectionState {
    pub fn new(
        notification_tx: mpsc::UnboundedSender<JsonRpcMessage>,
        pi_pool: Arc<PiPool>,
        thread_index: Arc<dyn ThreadIndexHandle>,
        defaults: ThreadDefaults,
    ) -> Self {
        Self {
            capabilities: Mutex::new(Capabilities::default()),
            defaults: Mutex::new(defaults),
            notification_tx,
            pending_server_requests: AsyncMutex::new(HashMap::new()),
            pi_pool,
            thread_index,
        }
    }

    /// Replace the negotiated capabilities. Called from `handlers::lifecycle`
    /// when `initialize` lands.
    pub fn set_capabilities(
        &self,
        client_name: Option<String>,
        client_title: Option<String>,
        client_version: Option<String>,
        caps: Option<&InitializeCapabilities>,
    ) {
        let mut slot = self.capabilities.lock().unwrap();
        slot.client_name = client_name;
        slot.client_title = client_title;
        slot.client_version = client_version;
        slot.experimental_api = caps.is_some_and(|c| c.experimental_api);
        slot.opt_out_notification_methods = caps
            .and_then(|c| c.opt_out_notification_methods.as_ref())
            .map(|v| v.iter().cloned().collect())
            .unwrap_or_default();
    }

    /// Snapshot the current capabilities. Cheap clone of a small struct.
    pub fn capabilities(&self) -> Capabilities {
        self.capabilities.lock().unwrap().clone()
    }

    /// True if the connection has not opted out of `method`.
    pub fn should_emit(&self, method: &str) -> bool {
        !self
            .capabilities
            .lock()
            .unwrap()
            .opt_out_notification_methods
            .contains(method)
    }

    /// Snapshot the bridge thread defaults.
    pub fn defaults(&self) -> ThreadDefaults {
        self.defaults.lock().unwrap().clone()
    }

    /// Mutate the bridge defaults under the lock.
    pub fn update_defaults(&self, f: impl FnOnce(&mut ThreadDefaults)) {
        let mut slot = self.defaults.lock().unwrap();
        f(&mut slot);
    }

    /// Send an outbound JSON-RPC frame (notification, response, or
    /// server→client request) to the client. Returns `Err` only if the
    /// writer task has already exited, which generally means the connection
    /// is shutting down.
    pub fn send(&self, msg: JsonRpcMessage) -> Result<(), SendError> {
        self.notification_tx
            .send(msg)
            .map_err(|_| SendError::ConnectionClosed)
    }

    /// Register an in-flight server→client request. Returns a oneshot the
    /// caller awaits for the client's response (or an error/timeout).
    pub async fn register_pending_request(
        &self,
        request_id: RequestId,
        method: String,
    ) -> oneshot::Receiver<Result<serde_json::Value, ServerRequestError>> {
        let (tx, rx) = oneshot::channel();
        self.pending_server_requests.lock().await.insert(
            request_id,
            PendingServerRequest {
                method,
                responder: tx,
            },
        );
        rx
    }

    /// Resolve an in-flight server→client request with the client's response.
    /// Returns `false` if no matching request is pending (already resolved or
    /// timed out).
    pub async fn resolve_pending_request(
        &self,
        request_id: &RequestId,
        result: Result<serde_json::Value, ServerRequestError>,
    ) -> bool {
        let Some(slot) = self.pending_server_requests.lock().await.remove(request_id) else {
            return false;
        };
        let _ = slot.responder.send(result);
        true
    }

    /// Cancel every outstanding server→client request, e.g. on connection
    /// shutdown. Each waiting handler receives `ConnectionClosed`.
    pub async fn cancel_all_pending_requests(&self) {
        let mut slot = self.pending_server_requests.lock().await;
        for (_id, pending) in slot.drain() {
            let _ = pending
                .responder
                .send(Err(ServerRequestError::ConnectionClosed));
        }
    }

    pub fn pi_pool(&self) -> &Arc<PiPool> {
        &self.pi_pool
    }

    pub fn thread_index(&self) -> &Arc<dyn ThreadIndexHandle> {
        &self.thread_index
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("connection writer is closed")]
    ConnectionClosed,
}
