//! Hermes target — spawn `alleycat-hermes-bridge` in stdio mode with
//! `HERMES_API_BASE` pointed at a running Hermes Agent gateway (or in
//! CLI mode via `HERMES_BRIDGE_BIN`).

use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use tempfile::TempDir;
use tokio::process::Command;

use super::{TargetHandle, TargetSpawn, tee_stream};
use crate::transport::{JsonRpcClient, boxed_reader, boxed_writer};

pub async fn spawn(opts: TargetSpawn) -> Result<TargetHandle> {
    let bridge_bin = opts
        .bridge_bin
        .ok_or_else(|| anyhow!("hermes target requires bridge_bin"))?;
    let codex_home = TempDir::new().context("codex home tempdir")?;

    let mut cmd = Command::new(&bridge_bin);
    cmd.env("CODEX_HOME", codex_home.path());

    // If a backend_bin was provided, set HERMES_BRIDGE_BIN so the bridge
    // uses CLI mode, unless the caller explicitly requested API mode via
    // HERMES_BRIDGE_MODE=api for gateway conformance.
    let explicit_api_mode = std::env::var("HERMES_BRIDGE_MODE")
        .map(|mode| mode.eq_ignore_ascii_case("api"))
        .unwrap_or(false);
    if let Some(ref backend_bin) = opts.backend_bin
        && !explicit_api_mode
    {
        cmd.env("HERMES_BRIDGE_BIN", backend_bin);
        cmd.env("HERMES_BRIDGE_MODE", "cli");
    }

    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {}", bridge_bin.display()))?;

    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    tee_stream("hermes-bridge", stderr);

    let client = JsonRpcClient::new(boxed_reader(stdout), boxed_writer(stdin));
    Ok(TargetHandle::new(
        client,
        Some(child),
        vec![codex_home],
        None,
    ))
}
