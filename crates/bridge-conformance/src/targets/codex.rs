//! Codex target — spawn `codex app-server` over stdio. Cleaner than going
//! through the alleycat TCP passthrough because (a) `codex` ships with both
//! a `--listen ws://...` and a stdio mode, and we don't have to negotiate
//! WebSocket framing for the test, and (b) we get a fresh process per test
//! with no port collisions.

use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use tokio::process::Command;

use super::{TargetHandle, TargetSpawn, tee_stream};
use crate::transport::{JsonRpcClient, boxed_reader, boxed_writer};

pub async fn spawn(opts: TargetSpawn) -> Result<TargetHandle> {
    let bin = opts
        .backend_bin
        .ok_or_else(|| anyhow!("codex target requires backend_bin"))?;

    let mut child = Command::new(&bin)
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {} app-server", bin.display()))?;

    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    tee_stream("codex", stderr);

    let client = JsonRpcClient::new(boxed_reader(stdout), boxed_writer(stdin));
    Ok(TargetHandle::new(client, Some(child), Vec::new(), None))
}
