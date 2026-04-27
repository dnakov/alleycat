//! Per-process PTY lifecycle for codex `command/exec`.
//!
//! Each `command/exec` request mints (or reuses) a `PtyProcess` that owns a
//! persistent websocket to opencode's `/pty/{id}/connect` endpoint, an output
//! buffer for buffered mode, and a `tokio::sync::oneshot` resolved by the SSE
//! `pty.exited` event. The buffered case awaits the oneshot and then returns
//! `{ exit_code, stdout, stderr:"" }`. The streaming case wires per-chunk
//! callers to a `broadcast` channel so the events translator can fan out
//! `command/exec/outputDelta` notifications keyed by `process_id`.
//!
//! Note: opencode's PTY transport does not separate stdout from stderr — the
//! pseudo-terminal multiplexes both onto a single byte stream. Codex's
//! `CommandExecResponse.stderr` is therefore always empty, matching the
//! contract of `tty:true` on the codex side.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};

use crate::opencode_client::OpencodeClient;

/// Maximum number of streamed chunks buffered per process. Subscribers that
/// fall behind will see broadcast::error::RecvError::Lagged frames; the SSE
/// handler logs and skips them.
const STREAM_CHANNEL_CAPACITY: usize = 256;

#[derive(Default)]
pub struct PtyState {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    by_process: HashMap<String, Arc<PtyProcess>>,
    pty_to_process: HashMap<String, String>,
}

pub struct PtyProcess {
    pub process_id: String,
    pub pty_id: String,
    /// Buffered stdout (PTY merges stderr into this stream).
    pub output: Mutex<Vec<u8>>,
    /// Streaming chunk publisher. Subscribed to by the SSE event router so
    /// streaming `command/exec` callers can emit `outputDelta` notifications.
    pub stream_tx: broadcast::Sender<Vec<u8>>,
    /// stdin write channel forwarded to the websocket task.
    write_tx: mpsc::UnboundedSender<WriteCommand>,
    /// Resolved by `pty.exited` (SSE) or websocket close fallback. Held in
    /// an `Option` so the resolver may take ownership exactly once.
    exit_tx: Mutex<Option<oneshot::Sender<i32>>>,
    /// Receivers waiting for the exit code subscribe via `take_exit_rx`.
    exit_rx: Mutex<Option<oneshot::Receiver<i32>>>,
}

enum WriteCommand {
    Bytes(Vec<u8>),
    CloseStdin,
}

impl PtyState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a freshly-created PTY. Spawns a background task that owns a
    /// long-lived websocket to opencode and routes input/output through the
    /// returned `PtyProcess` handle.
    pub fn register(
        &self,
        client: &OpencodeClient,
        process_id: String,
        pty_id: String,
    ) -> Arc<PtyProcess> {
        let (write_tx, write_rx) = mpsc::unbounded_channel::<WriteCommand>();
        let (exit_tx, exit_rx) = oneshot::channel::<i32>();
        let (stream_tx, _) = broadcast::channel::<Vec<u8>>(STREAM_CHANNEL_CAPACITY);
        let process = Arc::new(PtyProcess {
            process_id: process_id.clone(),
            pty_id: pty_id.clone(),
            output: Mutex::new(Vec::new()),
            stream_tx: stream_tx.clone(),
            write_tx,
            exit_tx: Mutex::new(Some(exit_tx)),
            exit_rx: Mutex::new(Some(exit_rx)),
        });
        {
            let mut inner = self.inner.lock().unwrap();
            inner
                .by_process
                .insert(process_id.clone(), Arc::clone(&process));
            inner
                .pty_to_process
                .insert(pty_id.clone(), process_id.clone());
        }
        let process_for_task = Arc::clone(&process);
        let client_for_task = client.clone();
        tokio::spawn(async move {
            if let Err(error) =
                drive_pty_websocket(&client_for_task, &process_for_task, write_rx).await
            {
                warn!(?error, pty_id = %process_for_task.pty_id, "pty websocket task ended with error");
            }
            // If the websocket closed before SSE delivered an exit, fall back
            // to exitCode -1 so the buffered caller does not hang forever.
            if let Some(tx) = process_for_task.exit_tx.lock().unwrap().take() {
                let _ = tx.send(-1);
            }
        });
        process
    }

    pub fn get_by_process(&self, process_id: &str) -> Option<Arc<PtyProcess>> {
        self.inner
            .lock()
            .unwrap()
            .by_process
            .get(process_id)
            .cloned()
    }

    /// Resolve a `pty.exited` SSE event. Idempotent: subsequent calls are a
    /// no-op (the oneshot has already been taken).
    pub fn finish(&self, pty_id: &str, exit_code: i32) {
        let process = {
            let inner = self.inner.lock().unwrap();
            inner
                .pty_to_process
                .get(pty_id)
                .and_then(|process_id| inner.by_process.get(process_id).cloned())
        };
        let Some(process) = process else { return };
        if let Some(tx) = process.exit_tx.lock().unwrap().take() {
            let _ = tx.send(exit_code);
        }
    }

    /// Remove a process from the registry. Called after a successful
    /// `command/exec/terminate` or after a buffered exec resolves and the
    /// caller has pulled stdout.
    pub fn remove(&self, process_id: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(process) = inner.by_process.remove(process_id) {
            inner.pty_to_process.remove(&process.pty_id);
        }
    }
}

impl PtyProcess {
    pub fn write(&self, bytes: Vec<u8>) -> anyhow::Result<()> {
        self.write_tx
            .send(WriteCommand::Bytes(bytes))
            .map_err(|err| anyhow::anyhow!("pty write channel closed: {err}"))
    }

    pub fn close_stdin(&self) -> anyhow::Result<()> {
        self.write_tx
            .send(WriteCommand::CloseStdin)
            .map_err(|err| anyhow::anyhow!("pty write channel closed: {err}"))
    }

    /// Take ownership of the exit-code receiver. Subsequent calls return
    /// `None`. Buffered execs await this to assemble their final response.
    pub fn take_exit_rx(&self) -> Option<oneshot::Receiver<i32>> {
        self.exit_rx.lock().unwrap().take()
    }

    pub fn snapshot_output(&self) -> Vec<u8> {
        self.output.lock().unwrap().clone()
    }

    pub fn subscribe_stream(&self) -> broadcast::Receiver<Vec<u8>> {
        self.stream_tx.subscribe()
    }
}

async fn drive_pty_websocket(
    client: &OpencodeClient,
    process: &PtyProcess,
    mut write_rx: mpsc::UnboundedReceiver<WriteCommand>,
) -> anyhow::Result<()> {
    let url = client.pty_connect_url(&process.pty_id);
    debug!(url, "pty connect");
    let (ws, _) = tokio_tungstenite::connect_async(&url).await?;
    let (mut sink, mut stream) = ws.split();
    loop {
        tokio::select! {
            biased;
            command = write_rx.recv() => {
                match command {
                    Some(WriteCommand::Bytes(bytes)) => {
                        // opencode's PTY connect channel reads stdin as text.
                        let text = String::from_utf8_lossy(&bytes).to_string();
                        if let Err(error) = sink.send(Message::Text(text.into())).await {
                            warn!(?error, "pty stdin send failed");
                            return Ok(());
                        }
                    }
                    Some(WriteCommand::CloseStdin) => {
                        // opencode does not surface a stdin-half-close on
                        // /pty/connect; closing the socket would also tear
                        // down output. Best effort: ignore.
                        debug!("close_stdin requested but not supported by opencode pty WS");
                    }
                    None => {
                        // Sender dropped — process registry removed us. Tear down.
                        let _ = sink.send(Message::Close(None)).await;
                        return Ok(());
                    }
                }
            }
            frame = stream.next() => {
                let Some(frame) = frame else {
                    return Ok(());
                };
                let frame = frame?;
                match frame {
                    Message::Text(text) => append_output(process, text.as_bytes().to_vec()),
                    Message::Binary(bytes) => {
                        let bytes = bytes.to_vec();
                        // opencode prefixes control frames (e.g. cursor metadata) with 0x00.
                        if bytes.first() == Some(&0u8) {
                            debug!("ignoring pty control frame");
                            continue;
                        }
                        append_output(process, bytes);
                    }
                    Message::Close(_) => return Ok(()),
                    Message::Ping(payload) => {
                        let _ = sink.send(Message::Pong(payload)).await;
                    }
                    _ => {}
                }
            }
        }
    }
}

fn append_output(process: &PtyProcess, bytes: Vec<u8>) {
    process.output.lock().unwrap().extend_from_slice(&bytes);
    let _ = process.stream_tx.send(bytes);
}

/// Build the codex `command/exec/outputDelta` notification payload for a
/// streamed chunk. Pure function for unit tests.
pub fn output_delta_payload(process_id: &str, stream: &str, bytes: &[u8]) -> Value {
    use base64::Engine;
    let delta_base64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    json!({
        "processId": process_id,
        "stream": stream,
        "deltaBase64": delta_base64,
        "capReached": false,
    })
}

#[cfg(test)]
impl PtyState {
    /// Test-only: register a `PtyProcess` without spawning a websocket task.
    /// The returned handle lets the test push output into `output`/`stream_tx`
    /// and resolve the exit oneshot via `PtyState::finish`. The write channel
    /// is wired but its receiver is dropped — `process.write(...)` will fail.
    pub(crate) fn register_for_test(
        &self,
        process_id: String,
        pty_id: String,
    ) -> Arc<PtyProcess> {
        let (write_tx, write_rx) = mpsc::unbounded_channel::<WriteCommand>();
        drop(write_rx);
        let (exit_tx, exit_rx) = oneshot::channel::<i32>();
        let (stream_tx, _) = broadcast::channel::<Vec<u8>>(STREAM_CHANNEL_CAPACITY);
        let process = Arc::new(PtyProcess {
            process_id: process_id.clone(),
            pty_id: pty_id.clone(),
            output: Mutex::new(Vec::new()),
            stream_tx,
            write_tx,
            exit_tx: Mutex::new(Some(exit_tx)),
            exit_rx: Mutex::new(Some(exit_rx)),
        });
        let mut inner = self.inner.lock().unwrap();
        inner
            .by_process
            .insert(process_id.clone(), Arc::clone(&process));
        inner.pty_to_process.insert(pty_id, process_id);
        process
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast::error::TryRecvError;

    #[test]
    fn output_delta_payload_base64_encodes_bytes() {
        let payload = output_delta_payload("p1", "stdout", b"hi");
        assert_eq!(payload["processId"], "p1");
        assert_eq!(payload["stream"], "stdout");
        assert_eq!(payload["deltaBase64"], "aGk=");
        assert_eq!(payload["capReached"], false);
    }

    #[tokio::test]
    async fn finish_resolves_exit_oneshot_and_buffered_output_survives() {
        let state = PtyState::new();
        let process = state.register_for_test("proc-1".to_string(), "pty-1".to_string());
        // Simulate websocket-arrived output chunks accumulating into `output`.
        append_output(&process, b"hello ".to_vec());
        append_output(&process, b"world".to_vec());

        // SSE delivers exit; the buffered caller's awaiter resolves with the code.
        state.finish("pty-1", 0);

        let exit_rx = process.take_exit_rx().expect("exit rx available");
        let exit_code = tokio::time::timeout(std::time::Duration::from_secs(1), exit_rx)
            .await
            .expect("exit rx should resolve")
            .expect("exit code");
        assert_eq!(exit_code, 0);
        assert_eq!(process.snapshot_output(), b"hello world");
    }

    #[tokio::test]
    async fn streaming_subscribers_receive_each_chunk_in_order() {
        let state = PtyState::new();
        let process = state.register_for_test("proc-2".to_string(), "pty-2".to_string());
        let mut rx = process.subscribe_stream();

        append_output(&process, b"a".to_vec());
        append_output(&process, b"bc".to_vec());

        let first = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("rx timeout")
            .expect("rx closed");
        assert_eq!(first, b"a");
        let second = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("rx timeout")
            .expect("rx closed");
        assert_eq!(second, b"bc");
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn finish_is_idempotent() {
        let state = PtyState::new();
        let _process = state.register_for_test("proc-3".to_string(), "pty-3".to_string());
        state.finish("pty-3", 7);
        // Second call must not panic and must not double-resolve the oneshot.
        state.finish("pty-3", 99);
    }

    #[test]
    fn remove_clears_both_indices() {
        let state = PtyState::new();
        let _process = state.register_for_test("proc-4".to_string(), "pty-4".to_string());
        assert!(state.get_by_process("proc-4").is_some());
        state.remove("proc-4");
        assert!(state.get_by_process("proc-4").is_none());
        // pty_id reverse lookup should be cleared too — finish becomes a no-op.
        state.finish("pty-4", 1);
    }
}
