//! Thin client subcommands that talk to the running daemon over the IPC
//! control socket. Each subcommand opens a fresh connection, writes a single
//! length-prefixed JSON request, reads a single response, and exits.

use anyhow::{Context, anyhow};

use crate::daemon::control::{Request, Response};
use crate::framing::{read_json_frame, write_json_frame};
use crate::ipc;

pub mod agents;
pub mod logs;
pub mod pair;
pub mod probe;
pub mod reload;
pub mod rotate;
pub mod status;
pub mod stop;

/// Send a single request to the daemon and read back the response.
/// Errors with a friendly hint if the daemon is not running.
pub async fn send(req: Request) -> anyhow::Result<Response> {
    let mut stream = ipc::connect().await.with_context(
        || "daemon not running. start it with `alleycat serve` or `alleycat install`.",
    )?;
    write_json_frame(&mut stream, &req)
        .await
        .context("writing control request")?;
    let resp: Response = read_json_frame(&mut stream)
        .await
        .context("reading control response")?;
    Ok(resp)
}

/// Decode the typed payload from a successful response. Bails with the
/// daemon-supplied error message when `ok` is false.
pub fn decode_data<T: serde::de::DeserializeOwned>(resp: Response) -> anyhow::Result<T> {
    if !resp.ok {
        return Err(anyhow!(resp.error.unwrap_or_else(|| "daemon error".into())));
    }
    let data = resp
        .data
        .ok_or_else(|| anyhow!("daemon returned empty data"))?;
    Ok(serde_json::from_value(data)?)
}

/// Bail when the daemon returned `ok=false`.
pub fn require_ok(resp: &Response) -> anyhow::Result<()> {
    if !resp.ok {
        return Err(anyhow!(
            resp.error.clone().unwrap_or_else(|| "daemon error".into())
        ));
    }
    Ok(())
}
