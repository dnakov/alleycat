//! Opencode target — spawn `alleycat-opencode-bridge --socket=<tmp>.sock`
//! (the bridge has no stdio mode) and connect via `UnixStream`. The bridge
//! itself auto-spawns `opencode serve` on first use; we just have to point
//! `OPENCODE_BRIDGE_BIN` at the resolved binary so it doesn't fall back to
//! `which opencode` inside a sandboxed test environment.

#[cfg(unix)]
use std::process::Stdio;
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use anyhow::{Context, anyhow};
use anyhow::{Result, bail};
#[cfg(unix)]
use tempfile::TempDir;
#[cfg(unix)]
use tokio::net::UnixStream;
#[cfg(unix)]
use tokio::process::Command;
#[cfg(unix)]
use tokio::time::sleep;

#[cfg(not(unix))]
use super::{TargetHandle, TargetSpawn};
#[cfg(unix)]
use super::{TargetHandle, TargetSpawn, tee_stream};
#[cfg(unix)]
use crate::transport::{JsonRpcClient, boxed_reader, boxed_writer};

#[cfg(unix)]
const SOCKET_CONNECT_DEADLINE: Duration = Duration::from_secs(15);
#[cfg(unix)]
const SOCKET_RETRY_INTERVAL: Duration = Duration::from_millis(100);

#[cfg(unix)]
pub async fn spawn(opts: TargetSpawn) -> Result<TargetHandle> {
    let bridge_bin = opts
        .bridge_bin
        .ok_or_else(|| anyhow!("opencode target requires bridge_bin"))?;
    let opencode_bin = opts
        .backend_bin
        .ok_or_else(|| anyhow!("opencode target requires backend_bin"))?;
    let socket_dir = TempDir::new().context("opencode socket tempdir")?;
    let socket_path = socket_dir.path().join("opencode.sock");

    let mut child = Command::new(&bridge_bin)
        .arg("--socket")
        .arg(&socket_path)
        .env("OPENCODE_BRIDGE_BIN", &opencode_bin)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {}", bridge_bin.display()))?;

    if let Some(stdout) = child.stdout.take() {
        tee_stream("opencode-bridge.stdout", stdout);
    }
    let stderr = child.stderr.take().expect("piped stderr");
    tee_stream("opencode-bridge", stderr);

    // Wait for the bridge to bind its socket. We poll with a short retry
    // because the bridge does an async opencode-runtime startup before it
    // calls bind().
    let stream = wait_for_socket(&socket_path).await?;
    let (read_half, write_half) = stream.into_split();
    let client = JsonRpcClient::new(boxed_reader(read_half), boxed_writer(write_half));

    Ok(TargetHandle::new(
        client,
        Some(child),
        vec![socket_dir],
        Some(socket_path),
    ))
}

#[cfg(not(unix))]
pub async fn spawn(_opts: TargetSpawn) -> Result<TargetHandle> {
    bail!("opencode conformance target uses Unix sockets, which are not available on Windows")
}

#[cfg(unix)]
async fn wait_for_socket(path: &std::path::Path) -> Result<UnixStream> {
    let started = std::time::Instant::now();
    loop {
        if started.elapsed() > SOCKET_CONNECT_DEADLINE {
            bail!(
                "timed out waiting for opencode-bridge to bind socket at {}",
                path.display()
            );
        }
        match UnixStream::connect(path).await {
            Ok(stream) => return Ok(stream),
            Err(_) => sleep(SOCKET_RETRY_INTERVAL).await,
        }
    }
}
