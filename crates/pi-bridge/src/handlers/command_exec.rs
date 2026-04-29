//! `command/exec` and follow-ups (`/terminate`, `/write`, `/resize`).
//!
//! **Divergence from the design plan.** The plan suggests routing
//! `command/exec` through the pi process owning `params.cwd` (pi `bash`).
//! That adds startup latency and pool pressure for what is typically a
//! one-shot diagnostic shell ("git status", "ls", etc.) the codex client
//! issues outside of any conversation. Per team-lead direction, the bridge
//! instead executes argv locally with [`tokio::process::Command`]. Approval
//! policy still gates risky commands via `approval.rs` (#18).
//!
//! V1 surface:
//! - `command/exec`: buffered or streamed (`streamStdoutStderr:true`).
//! - `command/exec/terminate`: kills a streaming process by `processId`.
//! - `command/exec/write` / `command/exec/resize`: rejected with the
//!   "unsupported by pi-bridge v1" message — PTY/stdin support is gated on a
//!   future iteration of pi `bash` itself (see plan "Out of scope").
//!
//! Process tracking is a connection-scoped table in [`ExecRegistry`]; the
//! bridge today serves one connection at a time (`Multi-client sharing` is
//! out of scope for v1), so a process-wide [`LazyLock`] is sufficient. When
//! UDS multiplexing lands the table moves into `state::ConnectionState`.

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::Duration;

use alleycat_bridge_core::{ChildProcess, ProcessRole, ProcessSpec, StdioMode};
use anyhow::Result;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use tokio::io::AsyncReadExt;
use tokio::sync::oneshot;
use tokio::time::timeout;

use crate::codex_proto as p;
use crate::state::ConnectionState;

/// Per-stream byte cap used when the request neither sets `output_bytes_cap`
/// nor `disable_output_cap`. Mirrors the codex default well enough for a
/// diagnostic shell.
const DEFAULT_OUTPUT_BYTES_CAP: usize = 256 * 1024;

/// Default timeout when the request neither sets `timeout_ms` nor
/// `disable_timeout`. Generous enough for typical shell probes; turn-driven
/// commands set their own deadline.
const DEFAULT_TIMEOUT_MS: i64 = 60_000;

/// Connection-scoped registry of live streaming `command/exec` processes,
/// keyed by `process_id`. Buffered (no `process_id`) executions don't appear
/// here — the call returns a buffered response without ever needing a lookup.
static EXEC_REGISTRY: LazyLock<ExecRegistry> = LazyLock::new(ExecRegistry::default);

#[derive(Default)]
struct ExecRegistry {
    inner: Mutex<HashMap<String, ExecHandle>>,
}

struct ExecHandle {
    /// Sent on `command/exec/terminate`. The exec task races this against
    /// child completion and kills the child on receipt.
    terminate_tx: oneshot::Sender<()>,
}

enum SupervisorResult {
    Exited(std::io::Result<std::process::ExitStatus>),
    TimedOut,
}

impl ExecRegistry {
    fn insert(&self, id: String, handle: ExecHandle) {
        self.inner.lock().unwrap().insert(id, handle);
    }

    /// Drop the entry and return the terminate sender (consumed by `terminate`).
    fn take(&self, id: &str) -> Option<ExecHandle> {
        self.inner.lock().unwrap().remove(id)
    }
}

// === handlers ==============================================================

pub async fn handle_command_exec(
    state: &Arc<ConnectionState>,
    params: p::CommandExecParams,
) -> Result<p::CommandExecResponse, ExecError> {
    if params.command.is_empty() {
        return Err(ExecError::InvalidParams("empty command argv".into()));
    }
    if params.tty {
        return Err(ExecError::Unsupported(
            "tty mode is not supported by pi-bridge v1".into(),
        ));
    }
    if params.stream_stdin {
        return Err(ExecError::Unsupported(
            "stream_stdin is not supported by pi-bridge v1".into(),
        ));
    }
    if params.stream_stdout_stderr && params.process_id.is_none() {
        return Err(ExecError::InvalidParams(
            "stream_stdout_stderr requires a client-supplied processId".into(),
        ));
    }
    if params.disable_output_cap && params.output_bytes_cap.is_some() {
        return Err(ExecError::InvalidParams(
            "disable_output_cap cannot be combined with output_bytes_cap".into(),
        ));
    }
    if params.disable_timeout && params.timeout_ms.is_some() {
        return Err(ExecError::InvalidParams(
            "disable_timeout cannot be combined with timeout_ms".into(),
        ));
    }

    let argv = params.command.clone();
    let env: Vec<(OsString, OsString)> = params
        .env
        .as_ref()
        .map(|e| {
            e.iter()
                .filter_map(|(k, v)| {
                    v.as_ref()
                        .map(|val| (OsString::from(k), OsString::from(val)))
                })
                .collect()
        })
        .unwrap_or_default();
    let spec = ProcessSpec {
        role: ProcessRole::ToolCommand,
        program: std::path::PathBuf::from(&argv[0]),
        args: argv[1..].iter().map(OsString::from).collect(),
        cwd: params.cwd.as_ref().map(std::path::PathBuf::from),
        env,
        stdin: StdioMode::Null,
        stdout: StdioMode::Piped,
        stderr: StdioMode::Piped,
    };
    let child = state
        .launcher()
        .launch(spec)
        .await
        .map_err(ExecError::spawn)?;

    let cap = if params.disable_output_cap {
        usize::MAX
    } else {
        params.output_bytes_cap.unwrap_or(DEFAULT_OUTPUT_BYTES_CAP)
    };
    let timeout_dur = if params.disable_timeout {
        None
    } else {
        let ms = params.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).max(0) as u64;
        Some(Duration::from_millis(ms))
    };

    if params.stream_stdout_stderr {
        // SAFETY: process_id presence already validated above.
        let process_id = params.process_id.clone().unwrap();
        run_streaming(state, child, process_id, cap, timeout_dur).await
    } else {
        run_buffered(child, cap, timeout_dur).await
    }
}

pub async fn handle_command_exec_terminate(
    _state: &Arc<ConnectionState>,
    params: p::CommandExecTerminateParams,
) -> p::CommandExecTerminateResponse {
    if let Some(handle) = EXEC_REGISTRY.take(&params.process_id) {
        // The exec task may already have completed (entry stayed because the
        // task crashed). Sending on a closed oneshot is a no-op.
        let _ = handle.terminate_tx.send(());
    }
    p::CommandExecTerminateResponse::default()
}

pub async fn handle_command_exec_write(
    _state: &Arc<ConnectionState>,
    _params: p::CommandExecWriteParams,
) -> Result<p::CommandExecWriteResponse, ExecError> {
    Err(ExecError::Unsupported(
        "command/exec/write is not supported by pi-bridge v1 (no PTY/stdin streaming)".into(),
    ))
}

pub async fn handle_command_exec_resize(
    _state: &Arc<ConnectionState>,
    _params: p::CommandExecResizeParams,
) -> Result<p::CommandExecResizeResponse, ExecError> {
    Err(ExecError::Unsupported(
        "command/exec/resize is not supported by pi-bridge v1 (no PTY)".into(),
    ))
}

// === implementation =======================================================

async fn run_buffered(
    mut child: Box<dyn ChildProcess>,
    cap: usize,
    timeout_dur: Option<Duration>,
) -> Result<p::CommandExecResponse, ExecError> {
    let mut stdout = child
        .take_stdout()
        .ok_or_else(|| ExecError::internal("child has no stdout pipe"))?;
    let mut stderr = child
        .take_stderr()
        .ok_or_else(|| ExecError::internal("child has no stderr pipe"))?;

    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = read_capped(&mut stdout, &mut buf, cap).await;
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = read_capped(&mut stderr, &mut buf, cap).await;
        buf
    });

    let exit_status = match timeout_dur {
        Some(dur) => match timeout(dur, child.wait()).await {
            Ok(status) => status.map_err(ExecError::wait)?,
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(ExecError::Timeout);
            }
        },
        None => child.wait().await.map_err(ExecError::wait)?,
    };

    let stdout_bytes = stdout_task.await.unwrap_or_default();
    let stderr_bytes = stderr_task.await.unwrap_or_default();

    Ok(p::CommandExecResponse {
        exit_code: exit_status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
    })
}

async fn run_streaming(
    state: &Arc<ConnectionState>,
    mut child: Box<dyn ChildProcess>,
    process_id: String,
    cap: usize,
    timeout_dur: Option<Duration>,
) -> Result<p::CommandExecResponse, ExecError> {
    let stdout = child
        .take_stdout()
        .ok_or_else(|| ExecError::internal("child has no stdout pipe"))?;
    let stderr = child
        .take_stderr()
        .ok_or_else(|| ExecError::internal("child has no stderr pipe"))?;

    let (terminate_tx, terminate_rx) = oneshot::channel::<()>();
    EXEC_REGISTRY.insert(process_id.clone(), ExecHandle { terminate_tx });

    let stdout_state = Arc::clone(state);
    let stdout_pid = process_id.clone();
    let stdout_task = tokio::spawn(async move {
        stream_to_client(
            stdout,
            stdout_state,
            stdout_pid,
            p::CommandExecOutputStream::Stdout,
            cap,
        )
        .await
    });
    let stderr_state = Arc::clone(state);
    let stderr_pid = process_id.clone();
    let stderr_task = tokio::spawn(async move {
        stream_to_client(
            stderr,
            stderr_state,
            stderr_pid,
            p::CommandExecOutputStream::Stderr,
            cap,
        )
        .await
    });

    // Move the child into a supervisor task so we can wait + kill it from a
    // single owning future, then signal completion through a oneshot. This
    // sidesteps the double-borrow issue tokio::Child has when select!'d
    // against the wait future.
    let (exit_tx, exit_rx) = oneshot::channel::<std::io::Result<std::process::ExitStatus>>();
    let supervisor_pid = process_id.clone();
    let supervisor = tokio::spawn(async move {
        let mut child = child;
        // Use boxed wait future and select! against terminate/timeout.
        // On non-`wait` branches we still need to call `child.kill()` —
        // that requires the wait future to be dropped first. We borrow
        // wait inside an inner block; cancelling the select arm drops
        // the borrow, freeing `child` for `kill().await`.
        let result: SupervisorResult = if let Some(dur) = timeout_dur {
            tokio::select! {
                res = child.wait() => SupervisorResult::Exited(res),
                _ = terminate_rx => {
                    let _ = child.kill().await;
                    SupervisorResult::Exited(child.wait().await)
                }
                _ = tokio::time::sleep(dur) => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    SupervisorResult::TimedOut
                }
            }
        } else {
            tokio::select! {
                res = child.wait() => SupervisorResult::Exited(res),
                _ = terminate_rx => {
                    let _ = child.kill().await;
                    SupervisorResult::Exited(child.wait().await)
                }
            }
        };
        match result {
            SupervisorResult::Exited(res) => {
                let _ = exit_tx.send(res);
            }
            SupervisorResult::TimedOut => {
                EXEC_REGISTRY.take(&supervisor_pid);
                let _ = exit_tx.send(Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "command timed out",
                )));
            }
        }
    });

    let exit_result = exit_rx.await.map_err(|_| {
        ExecError::internal("exec supervisor task panicked before reporting status")
    })?;
    // Drain the supervisor task so any drop side effects flush before we
    // return.
    let _ = supervisor.await;

    let exit_status = match exit_result {
        Ok(status) => status,
        Err(err) if err.kind() == std::io::ErrorKind::TimedOut => {
            return Err(ExecError::Timeout);
        }
        Err(err) => return Err(ExecError::wait(err)),
    };

    // Drain the stream tasks so any buffered tail bytes flush before we send
    // the buffered response.
    let _ = stdout_task.await;
    let _ = stderr_task.await;

    // Drop our entry once the process has exited; subsequent terminate calls
    // are no-ops.
    EXEC_REGISTRY.take(&process_id);

    // When streaming, the buffered response carries empty stdout/stderr per
    // codex contract (`v2.rs:3186-3197` — "Empty when stdout was streamed").
    Ok(p::CommandExecResponse {
        exit_code: exit_status.code().unwrap_or(-1),
        stdout: String::new(),
        stderr: String::new(),
    })
}

async fn stream_to_client<R>(
    mut reader: R,
    state: Arc<ConnectionState>,
    process_id: String,
    stream: p::CommandExecOutputStream,
    cap: usize,
) where
    R: AsyncReadExt + Unpin,
{
    let mut buf = vec![0u8; 8 * 1024];
    let mut emitted: usize = 0;
    let mut cap_reached = false;

    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) => {
                tracing::warn!(%err, ?stream, "exec stream read failed");
                break;
            }
        };

        let allowed = cap.saturating_sub(emitted);
        if allowed == 0 {
            cap_reached = true;
            // Drain remainder so the child can exit, but stop emitting.
            continue;
        }
        let to_send = n.min(allowed);
        emitted += to_send;
        if to_send < n {
            cap_reached = true;
        }

        if !state.should_emit("command/exec/outputDelta") {
            continue;
        }
        let chunk = &buf[..to_send];
        let payload = p::CommandExecOutputDeltaNotification {
            process_id: process_id.clone(),
            stream,
            delta_base64: BASE64.encode(chunk),
            cap_reached,
        };
        let frame =
            match notification_message(&p::ServerNotification::CommandExecOutputDelta(payload)) {
                Ok(f) => f,
                Err(err) => {
                    tracing::warn!(%err, "failed to encode CommandExecOutputDelta");
                    continue;
                }
            };
        if state.send(frame).is_err() {
            // Connection closed; nothing left to do.
            return;
        }
        if cap_reached {
            // Emit a final empty chunk with cap_reached=true so the client
            // knows further bytes were truncated. Already covered above when
            // to_send < n, so we don't double-emit here.
        }
    }
}

async fn read_capped<R>(reader: &mut R, dest: &mut Vec<u8>, cap: usize) -> std::io::Result<()>
where
    R: AsyncReadExt + Unpin,
{
    let mut buf = vec![0u8; 8 * 1024];
    while dest.len() < cap {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let remaining = cap - dest.len();
        let take = n.min(remaining);
        dest.extend_from_slice(&buf[..take]);
        if take < n {
            // Truncated; keep draining so the child can exit cleanly.
            loop {
                let drained = reader.read(&mut buf).await?;
                if drained == 0 {
                    break;
                }
            }
            break;
        }
    }
    Ok(())
}

fn notification_message(
    notif: &p::ServerNotification,
) -> Result<p::JsonRpcMessage, serde_json::Error> {
    let value = serde_json::to_value(notif)?;
    let method = value
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or_default()
        .to_string();
    let params = value.get("params").cloned();
    Ok(p::JsonRpcMessage::Notification(p::JsonRpcNotification {
        jsonrpc: p::JsonRpcVersion,
        method,
        params,
    }))
}

// === errors ===============================================================

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("invalid params: {0}")]
    InvalidParams(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("command timed out")]
    Timeout,
    #[error("spawn failed: {0}")]
    Spawn(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl ExecError {
    fn spawn(err: std::io::Error) -> Self {
        Self::Spawn(err.to_string())
    }

    fn wait(err: std::io::Error) -> Self {
        Self::Internal(format!("waiting on child: {err}"))
    }

    fn internal<E: std::fmt::Display>(err: E) -> Self {
        Self::Internal(err.to_string())
    }

    /// JSON-RPC error code suitable for surfacing back through the
    /// dispatcher in main.rs.
    pub fn rpc_code(&self) -> i64 {
        match self {
            ExecError::InvalidParams(_) => p::error_codes::INVALID_PARAMS,
            ExecError::Unsupported(_) => p::error_codes::METHOD_NOT_FOUND,
            ExecError::Timeout | ExecError::Spawn(_) | ExecError::Internal(_) => {
                p::error_codes::INTERNAL_ERROR
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    async fn dummy_state() -> Arc<ConnectionState> {
        let dir = tempfile::tempdir().unwrap();
        let index = crate::index::ThreadIndex::open_at(dir.path().join("threads.json"))
            .await
            .unwrap();
        // tempdir auto-cleanup is fine for these tests — we don't restart a
        // ConnectionState across runs and threads.json is rebuilt each call.
        std::mem::forget(dir);
        let (state, _rx) = ConnectionState::for_test(
            Arc::new(crate::pool::PiPool::new("/usr/bin/false")),
            index,
            Default::default(),
        );
        state
    }

    #[tokio::test]
    async fn buffered_exec_returns_stdout() {
        let state = dummy_state().await;
        let resp = handle_command_exec(
            &state,
            p::CommandExecParams {
                command: vec!["sh".into(), "-c".into(), "printf hello".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(resp.exit_code, 0);
        assert_eq!(resp.stdout, "hello");
        assert_eq!(resp.stderr, "");
    }

    #[tokio::test]
    async fn buffered_exec_captures_stderr_separately() {
        let state = dummy_state().await;
        let resp = handle_command_exec(
            &state,
            p::CommandExecParams {
                command: vec!["sh".into(), "-c".into(), "printf err >&2; exit 3".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(resp.exit_code, 3);
        assert_eq!(resp.stdout, "");
        assert_eq!(resp.stderr, "err");
    }

    #[tokio::test]
    async fn timeout_kills_long_running_command() {
        let state = dummy_state().await;
        let err = handle_command_exec(
            &state,
            p::CommandExecParams {
                command: vec!["sh".into(), "-c".into(), "sleep 5".into()],
                timeout_ms: Some(50),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ExecError::Timeout), "got {err:?}");
    }

    #[tokio::test]
    async fn streaming_requires_process_id() {
        let state = dummy_state().await;
        let err = handle_command_exec(
            &state,
            p::CommandExecParams {
                command: vec!["true".into()],
                stream_stdout_stderr: true,
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ExecError::InvalidParams(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn write_and_resize_are_unsupported() {
        let state = dummy_state().await;
        let err = handle_command_exec_write(
            &state,
            p::CommandExecWriteParams {
                process_id: "p1".into(),
                delta_base64: None,
                close_stdin: false,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ExecError::Unsupported(_)), "got {err:?}");
        let err = handle_command_exec_resize(
            &state,
            p::CommandExecResizeParams {
                process_id: "p1".into(),
                size: p::CommandExecTerminalSize { rows: 24, cols: 80 },
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ExecError::Unsupported(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn terminate_unknown_process_is_noop() {
        let state = dummy_state().await;
        let _ = handle_command_exec_terminate(
            &state,
            p::CommandExecTerminateParams {
                process_id: "does-not-exist".into(),
            },
        )
        .await;
    }

    #[tokio::test]
    async fn streaming_emits_outputdelta_for_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let index = crate::index::ThreadIndex::open_at(dir.path().join("threads.json"))
            .await
            .unwrap();
        std::mem::forget(dir);
        let (state, mut rx) = ConnectionState::for_test(
            Arc::new(crate::pool::PiPool::new("/usr/bin/false")),
            index,
            Default::default(),
        );
        let resp = handle_command_exec(
            &state,
            p::CommandExecParams {
                command: vec!["sh".into(), "-c".into(), "printf chunk".into()],
                process_id: Some("pid-stream".into()),
                stream_stdout_stderr: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(resp.exit_code, 0);
        assert_eq!(resp.stdout, "");
        // The stream task drains via the unbounded channel; pull any frames
        // that landed and assert at least one outputDelta exists with stdout
        // stream and a base64 of "chunk".
        let mut saw_stdout_chunk = false;
        while let Ok(msg) = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
            let Some(seq) = msg else { break };
            let value = seq.payload;
            if value.get("method") == Some(&json!("command/exec/outputDelta")) {
                let params = value.get("params").unwrap();
                if params.get("stream") == Some(&json!("stdout"))
                    && params.get("deltaBase64") == Some(&json!(BASE64.encode("chunk")))
                {
                    saw_stdout_chunk = true;
                    break;
                }
            }
        }
        assert!(
            saw_stdout_chunk,
            "expected a stdout outputDelta with `chunk`"
        );
    }
}
