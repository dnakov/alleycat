//! Per-target spawn / connect logic. Each module here turns a [`TargetSpawn`]
//! into a [`TargetHandle`] containing a connected [`JsonRpcClient`] plus the
//! resources (child processes, tempdirs, socket files) that have to live as
//! long as the test does.

use std::path::PathBuf;

use anyhow::Result;
use tokio::process::Child;

use crate::TargetId;
use crate::transport::JsonRpcClient;

pub mod claude;
pub mod codex;
pub mod opencode;
pub mod pi;

/// Inputs every target spawn needs. Most fields are target-specific; we keep
/// them in a single struct so the test entry-point looks the same regardless
/// of which target it's running.
#[derive(Debug, Clone)]
pub struct TargetSpawn {
    pub target: TargetId,
    /// Path to `alleycat-{pi,claude,opencode}-bridge`. Required for the
    /// three bridge targets; ignored for codex (which we drive directly).
    pub bridge_bin: Option<PathBuf>,
    /// Path to the backend CLI (`codex`, `pi`, `claude`, `opencode`).
    /// Forwarded to the bridge via the appropriate env var, or invoked
    /// directly for codex.
    pub backend_bin: Option<PathBuf>,
    /// Tempdir to use as `cwd` for the scenario's `thread/start`. The runner
    /// also points target-specific home/config env vars at fresh tempdirs.
    pub cwd: PathBuf,
}

/// Live connection + the resources backing it. `Drop` reaps the child and
/// removes any tempfiles.
pub struct TargetHandle {
    pub client: JsonRpcClient,
    // Held for ownership; the field is otherwise unused. `kill_on_drop` on
    // the Command takes care of cleanup.
    _child: Option<Child>,
    _tempdirs: Vec<tempfile::TempDir>,
    _socket: Option<PathBuf>,
}

impl TargetHandle {
    pub fn new(
        client: JsonRpcClient,
        child: Option<Child>,
        tempdirs: Vec<tempfile::TempDir>,
        socket: Option<PathBuf>,
    ) -> Self {
        Self {
            client,
            _child: child,
            _tempdirs: tempdirs,
            _socket: socket,
        }
    }
}

impl Drop for TargetHandle {
    fn drop(&mut self) {
        if let Some(socket) = &self._socket {
            // Best-effort unlink — ignore errors.
            let _ = std::fs::remove_file(socket);
        }
    }
}

pub async fn spawn(opts: TargetSpawn) -> Result<TargetHandle> {
    match opts.target {
        TargetId::Codex => codex::spawn(opts).await,
        TargetId::Pi => pi::spawn(opts).await,
        TargetId::Claude => claude::spawn(opts).await,
        TargetId::Opencode => opencode::spawn(opts).await,
    }
}

/// Tee any line-oriented child stream (stdout or stderr) to the test's
/// stderr so failures show backend logs.
pub(crate) fn tee_stream<R>(label: &'static str, stream: R)
where
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
{
    use tokio::io::{AsyncBufReadExt, BufReader};
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("[{label}] {line}");
        }
    });
}
