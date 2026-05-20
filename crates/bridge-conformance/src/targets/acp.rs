//! ACP target — spawn `alleycat-acp-bridge` in stdio mode with
//! `ACP_BRIDGE_AGENT_BIN` + `ACP_BRIDGE_AGENT_ARGS` pointed at an ACP-compliant agent
//! (e.g. `devin` with "acp", or `grok` with "agent stdio").

use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use tokio::process::Command;

use super::{TargetHandle, TargetSpawn, tee_stream};
use crate::transport::{JsonRpcClient, boxed_reader, boxed_writer};

pub async fn spawn(opts: TargetSpawn) -> Result<TargetHandle> {
    let bridge_bin = opts
        .bridge_bin
        .ok_or_else(|| anyhow!("acp target requires bridge_bin"))?;
    let agent_bin = opts
        .backend_bin
        .ok_or_else(|| anyhow!("acp target requires backend_bin"))?;

    // Allow overriding the args for agents that don't use "acp" (e.g. grok uses "agent stdio").
    let agent_args = std::env::var("ACP_BRIDGE_AGENT_ARGS").unwrap_or_else(|_| "acp".to_string());

    let mut child = Command::new(&bridge_bin)
        .env("ACP_BRIDGE_AGENT_BIN", &agent_bin)
        .env("ACP_BRIDGE_AGENT_ARGS", &agent_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {}", bridge_bin.display()))?;

    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    tee_stream("acp-bridge", stderr);

    let client = JsonRpcClient::new(boxed_reader(stdout), boxed_writer(stdin));
    Ok(TargetHandle::new(client, Some(child), vec![], None))
}
