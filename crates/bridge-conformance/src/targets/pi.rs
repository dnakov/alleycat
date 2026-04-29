//! Pi target — spawn `alleycat-pi-bridge` in stdio mode with
//! `PI_BRIDGE_PI_BIN` pointed at a real `pi-coding-agent`.

use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use tempfile::TempDir;
use tokio::process::Command;

use super::{TargetHandle, TargetSpawn, tee_stream};
use crate::transport::{JsonRpcClient, boxed_reader, boxed_writer};

pub async fn spawn(opts: TargetSpawn) -> Result<TargetHandle> {
    let bridge_bin = opts
        .bridge_bin
        .ok_or_else(|| anyhow!("pi target requires bridge_bin"))?;
    let pi_bin = opts
        .backend_bin
        .ok_or_else(|| anyhow!("pi target requires backend_bin"))?;
    let codex_home = TempDir::new().context("codex home tempdir")?;

    let mut child = Command::new(&bridge_bin)
        .env("PI_BRIDGE_PI_BIN", &pi_bin)
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
    tee_stream("pi-bridge", stderr);

    let client = JsonRpcClient::new(boxed_reader(stdout), boxed_writer(stdin));
    Ok(TargetHandle::new(
        client,
        Some(child),
        vec![codex_home],
        None,
    ))
}
