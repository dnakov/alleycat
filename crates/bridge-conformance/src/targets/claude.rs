//! Claude target — spawn `alleycat-claude-bridge` in stdio mode with
//! `CLAUDE_BRIDGE_CLAUDE_BIN` pointed at a real `claude` CLI.

use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use tempfile::TempDir;
use tokio::process::Command;

use super::{TargetHandle, TargetSpawn, tee_stream};
use crate::transport::{JsonRpcClient, boxed_reader, boxed_writer};

pub async fn spawn(opts: TargetSpawn) -> Result<TargetHandle> {
    let bridge_bin = opts
        .bridge_bin
        .ok_or_else(|| anyhow!("claude target requires bridge_bin"))?;
    let claude_bin = opts
        .backend_bin
        .ok_or_else(|| anyhow!("claude target requires backend_bin"))?;
    let codex_home = TempDir::new().context("codex home tempdir")?;

    // Note: we do NOT override CLAUDE_PROJECTS_DIR. Real `claude` always
    // writes to `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`; the
    // bridge reads from the same path. Overriding the bridge's view (but
    // not claude's) would just point at an empty dir and `thread/read`
    // would never find any turns. Each turn uses a fresh tempdir cwd, so
    // sessions are unique to this run.
    let mut child = Command::new(&bridge_bin)
        .env("CLAUDE_BRIDGE_CLAUDE_BIN", &claude_bin)
        .env("CODEX_HOME", codex_home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {}", bridge_bin.display()))?;

    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    tee_stream("claude-bridge", stderr);

    let client = JsonRpcClient::new(boxed_reader(stdout), boxed_writer(stdin));
    Ok(TargetHandle::new(
        client,
        Some(child),
        vec![codex_home],
        None,
    ))
}
