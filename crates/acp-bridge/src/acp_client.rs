//! ACP client for communicating with ACP-compliant agents over stdio.
//!
//! Architecture:
//!
//! * One background reader task owns the agent's stdout for the
//!   lifetime of the client and demuxes every line:
//!   * Frames with an `id` are matched to the currently-outstanding
//!     `send_request` and delivered through a `oneshot::Sender<Value>`.
//!   * Frames without an `id` (JSON-RPC notifications) fire the
//!     currently-registered notification subscriber if any, and are
//!     also pushed onto a fallback buffer so callers using
//!     `take_pending_notifications` see them.
//!
//! * Only one in-flight `send_request` is permitted at a time. We
//!   serialize via the stdin mutex (and a parallel "in-flight slot"
//!   mutex). ACP doesn't tag notifications with which request they
//!   correspond to, so deliberately serializing requests avoids
//!   ambiguity — notifications between request and response are
//!   unambiguously attributable to the in-flight request.
//!
//! * `send_request_streaming` lets the caller observe each notification
//!   as it arrives (instead of waiting for the response and then
//!   draining a buffer). `handle_turn_start` uses this to emit codex
//!   `item/*` notifications live as the ACP agent streams its output,
//!   so iOS sees the assistant text and tool bubbles appear in real
//!   time instead of all at once at the end of the turn.

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use alleycat_bridge_core::{ChildProcess, ProcessLauncher, ProcessRole, ProcessSpec, StdioMode};
use anyhow::Result;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::config::AcpBridgeConfig;

static REQUEST_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Shared mutable state owned by both the background reader task and
/// public client methods. The reader task holds an `Arc<Inner>` for the
/// lifetime of the agent process.
struct Inner {
    /// Outstanding `send_request` calls keyed by JSON-RPC id. Each entry
    /// is a oneshot the reader fulfills when the matching response
    /// arrives. Multiple entries are allowed in principle, but in
    /// practice we serialize requests via `request_lock`.
    pending: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    /// Optional live-notification subscriber installed by
    /// `send_request_streaming`. The reader task forwards every
    /// notification frame here while one is registered.
    notification_tx: Mutex<Option<mpsc::UnboundedSender<Value>>>,
    /// Fallback buffer for `take_pending_notifications` — populated for
    /// every notification regardless of whether a streaming subscriber
    /// is registered. Callers that *only* want streaming should drain
    /// this once after their request to discard the duplicates.
    pending_notifications: Mutex<Vec<Value>>,
}

impl Inner {
    fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            notification_tx: Mutex::new(None),
            pending_notifications: Mutex::new(Vec::new()),
        }
    }

    /// Route an inbound JSON frame.
    async fn dispatch(&self, frame: Value) {
        if let Some(id_val) = frame.get("id") {
            let id = match id_val.as_str() {
                Some(s) => s.to_string(),
                None => id_val.to_string(),
            };
            let mut map = self.pending.lock().await;
            if let Some(tx) = map.remove(&id) {
                let _ = tx.send(frame);
            } else {
                // No one is waiting on this id — drop with a warning. ACP
                // agents shouldn't send unsolicited responses.
                warn!(id, "received response for unknown request id; dropping");
            }
            return;
        }
        // Notification: forward to live subscriber + buffer.
        let buffered = frame.clone();
        if let Some(tx) = self.notification_tx.lock().await.as_ref() {
            if tx.send(frame).is_err() {
                // Subscriber went away — clear it so we stop trying.
                *self.notification_tx.lock().await = None;
            }
        }
        self.pending_notifications.lock().await.push(buffered);
    }
}

/// ACP client that communicates with an ACP agent over stdio.
pub struct AcpClient {
    process: Arc<Mutex<Box<dyn ChildProcess>>>,
    stdin: Arc<Mutex<alleycat_bridge_core::ChildStdin>>,
    inner: Arc<Inner>,
    /// Serializes outstanding requests. ACP notifications aren't tagged
    /// with which in-flight request they belong to, so we deliberately
    /// run one request at a time.
    request_lock: Arc<Mutex<()>>,
    /// JoinHandle for the background reader so we can abort it on
    /// `kill()` instead of leaking the task.
    reader_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl AcpClient {
    /// Spawn a new ACP agent process and create a client for it.
    pub async fn spawn(
        config: &AcpBridgeConfig,
        launcher: &Arc<dyn ProcessLauncher>,
    ) -> Result<Self> {
        let args: Vec<OsString> = config
            .agent_args
            .iter()
            .map(|s| OsString::from(s.as_str()))
            .collect();

        // stderr is set to Null on purpose: ACP agents (devin in
        // particular) emit a steady stream of tracing output. A
        // Piped+unread stderr deadlocks the child once the OS pipe
        // buffer fills (~64KB on macOS) — child blocks on stderr write,
        // stops draining stdin, and the bridge then hangs.
        let spec = ProcessSpec {
            program: config.agent_bin.clone(),
            args,
            role: ProcessRole::Agent,
            cwd: None,
            env: vec![],
            env_clear: false,
            stdin: StdioMode::Piped,
            stdout: StdioMode::Piped,
            stderr: StdioMode::Null,
        };

        info!(?spec, "spawning ACP agent process");
        let mut process = launcher.launch(spec).await?;

        let stdin = process
            .take_stdin()
            .ok_or_else(|| anyhow::anyhow!("ACP agent has no stdin pipe"))?;
        let stdout = process
            .take_stdout()
            .ok_or_else(|| anyhow::anyhow!("ACP agent has no stdout pipe"))?;

        let inner = Arc::new(Inner::new());
        let reader_inner = Arc::clone(&inner);
        let handle = tokio::spawn(async move {
            reader_task(BufReader::new(stdout), reader_inner).await;
        });

        Ok(Self {
            process: Arc::new(Mutex::new(process)),
            stdin: Arc::new(Mutex::new(stdin)),
            inner,
            request_lock: Arc::new(Mutex::new(())),
            reader_handle: Arc::new(Mutex::new(Some(handle))),
        })
    }

    /// Drain notifications buffered since the last call. Kept for
    /// callers (currently `handle_thread_resume`'s `session/load` drain
    /// path) that prefer the post-response batch model over streaming.
    pub async fn take_pending_notifications(&self) -> Vec<Value> {
        let mut guard = self.inner.pending_notifications.lock().await;
        std::mem::take(&mut *guard)
    }

    /// Send a JSON-RPC request and wait for the response.
    pub async fn send_request(&self, method: &str, params: Value) -> Result<Value> {
        self.send_request_inner(method, params, None).await
    }

    /// Send a JSON-RPC request and fire `on_notification(value)` for
    /// every notification frame received between the request being
    /// written and the response arriving. Notifications received after
    /// the response are NOT delivered to `on_notification`; they remain
    /// in `take_pending_notifications`.
    pub async fn send_request_streaming<F>(
        &self,
        method: &str,
        params: Value,
        mut on_notification: F,
    ) -> Result<Value>
    where
        F: FnMut(Value) + Send + 'static,
    {
        let (note_tx, mut note_rx) = mpsc::unbounded_channel::<Value>();
        // Run the notification consumer on its own task so it can keep
        // up while we're blocked on the response oneshot.
        let consumer = tokio::spawn(async move {
            while let Some(v) = note_rx.recv().await {
                on_notification(v);
            }
        });
        let result = self.send_request_inner(method, params, Some(note_tx)).await;
        // Closing the subscriber drops note_tx (unregistered inside
        // send_request_inner), which ends note_rx, which ends `consumer`.
        let _ = consumer.await;
        result
    }

    async fn send_request_inner(
        &self,
        method: &str,
        params: Value,
        notification_tx: Option<mpsc::UnboundedSender<Value>>,
    ) -> Result<Value> {
        // Serialize: only one in-flight request per client.
        let _slot = self.request_lock.lock().await;

        let request_id = REQUEST_ID_COUNTER
            .fetch_add(1, Ordering::SeqCst)
            .to_string();
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params,
        });

        // Register oneshot BEFORE writing so a fast response doesn't race us.
        let (tx, rx) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .await
            .insert(request_id.clone(), tx);

        // Install live notification subscriber if requested. Cleared
        // automatically on the way out.
        if let Some(sub) = notification_tx {
            *self.inner.notification_tx.lock().await = Some(sub);
        }

        // Drain any stale buffered notifications from before this
        // request — they belong to whatever happened earlier and would
        // pollute streaming consumers.
        self.inner.pending_notifications.lock().await.clear();

        debug!(method, request_id, "sending ACP request");

        let request_line = serde_json::to_string(&request)?;
        let write_result = async {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(request_line.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await?;
            Ok::<_, std::io::Error>(())
        }
        .await;
        if let Err(err) = write_result {
            // Clean up the pending registration if the write failed.
            self.inner.pending.lock().await.remove(&request_id);
            *self.inner.notification_tx.lock().await = None;
            return Err(err.into());
        }

        // Wait for the response. The reader task will route the matching
        // frame here once it lands.
        let response = match rx.await {
            Ok(v) => v,
            Err(_) => {
                self.inner.pending.lock().await.remove(&request_id);
                *self.inner.notification_tx.lock().await = None;
                anyhow::bail!("ACP agent connection closed before response");
            }
        };

        // Clear streaming subscriber so it doesn't receive frames from
        // the NEXT request.
        *self.inner.notification_tx.lock().await = None;

        debug!(method, request_id, "received ACP response");

        if let Some(error) = response.get("error") {
            error!(?error, "ACP agent returned error");
            // Surface just the human-readable `message` so callers (and
            // ultimately the iOS error toast) see a clean line.
            let message = error
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("ACP agent returned an error");
            anyhow::bail!("{message}");
        }

        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Send a JSON-RPC notification (no response expected).
    pub async fn send_notification(&self, method: &str, params: Value) -> Result<()> {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });

        debug!(method, "sending ACP notification");

        let notification_line = serde_json::to_string(&notification)?;
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(notification_line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;

        Ok(())
    }

    /// Kill the underlying agent process and abort the reader task.
    pub async fn kill(&self) -> Result<()> {
        if let Some(handle) = self.reader_handle.lock().await.take() {
            handle.abort();
        }
        let mut process = self.process.lock().await;
        process
            .kill()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to kill process: {}", e))
    }
}

/// Background loop: read newline-delimited JSON frames from the agent
/// and dispatch each one through `Inner`.
async fn reader_task(mut reader: BufReader<alleycat_bridge_core::ChildStdout>, inner: Arc<Inner>) {
    loop {
        let mut line = String::new();
        let n = match reader.read_line(&mut line).await {
            Ok(n) => n,
            Err(err) => {
                error!(?err, "error reading from ACP agent stdout");
                break;
            }
        };
        if n == 0 {
            debug!("ACP agent stdout closed");
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(frame) => inner.dispatch(frame).await,
            Err(err) => {
                warn!(
                    ?err,
                    line = trimmed,
                    "malformed JSON from ACP agent; dropping"
                );
            }
        }
    }
    // Wake up any outstanding request so callers don't hang forever.
    let mut pending = inner.pending.lock().await;
    for (_id, tx) in pending.drain() {
        // Synthesize an error response so the request fails cleanly.
        let _ = tx.send(serde_json::json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": {"code": -32000, "message": "ACP agent connection closed"},
        }));
    }
}
