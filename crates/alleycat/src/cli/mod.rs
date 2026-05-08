//! Thin client subcommands that talk to the running daemon over the IPC
//! control socket. Each subcommand opens a fresh connection, writes a single
//! length-prefixed JSON request, reads a single response, and exits.

use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};

use crate::daemon::control::{Request, Response, StatusInfo};
use crate::framing::{read_json_frame, write_json_frame};
use crate::ipc;

pub mod agents;
pub mod logs;
pub mod onboarding;
pub mod pair;
pub mod probe;
pub mod reload;
pub mod rotate;
pub mod status;
pub mod stop;
pub mod upgrade;

/// Send a single request to the daemon and read back the response.
/// Errors with a friendly hint if the daemon is not running.
pub async fn send(req: Request) -> anyhow::Result<Response> {
    let mut stream = ipc::connect().await.with_context(|| {
        let name = crate::binary_name();
        format!("daemon not running. start it with `{name} serve` or `{name} install`.")
    })?;
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

/// If a daemon is running on a *different* binary version than this CLI,
/// gracefully stop it and respawn `current_exe serve` as a detached child.
/// No-op when the daemon isn't running, when versions match, or when we
/// can't tell (older daemon that doesn't advertise `version`).
///
/// Called at the top of subcommands that mutate user-visible state (`pair`,
/// `rotate`) so a freshly-installed CLI never silently talks to a stale
/// daemon and produces stale output (e.g. a pair QR missing the relay URL).
pub async fn ensure_current_daemon() -> anyhow::Result<()> {
    if !ipc::is_daemon_running().await {
        return Ok(());
    }
    let Ok(resp) = send(Request::Status).await else {
        return Ok(());
    };
    let Ok(status) = decode_data::<StatusInfo>(resp) else {
        return Ok(());
    };
    let cli_version = crate::binary_version();
    let daemon_version = match status.version.as_deref() {
        Some(v) => v,
        // Pre-`version` daemons can't tell us — assume mismatch and offer
        // to bounce, since the only way the user invoked this code path is
        // by running a newer CLI.
        None => "<unknown>",
    };
    if daemon_version == cli_version {
        return Ok(());
    }
    eprintln!(
        "note: daemon is v{daemon_version} but {cli} is v{cli_version}; restarting daemon onto v{cli_version}...",
        cli = crate::binary_name()
    );
    restart_daemon().await
}

/// Stop any running daemon and start a fresh one from `current_exe`.
/// Public so the explicit `upgrade` subcommand can reuse the same path.
pub async fn restart_daemon() -> anyhow::Result<()> {
    if ipc::is_daemon_running().await {
        let _ = send(Request::Stop).await;
        wait_until(Duration::from_secs(10), || async {
            !ipc::is_daemon_running().await
        })
        .await
        .context("old daemon did not exit within 10s")?;
    }
    spawn_serve_detached().context("spawning new daemon")?;
    wait_until(Duration::from_secs(15), || async {
        ipc::is_daemon_running().await
    })
    .await
    .context("new daemon did not come up within 15s")?;
    Ok(())
}

/// Spawn `current_exe serve` as a session-detached background process so
/// it survives the parent CLI exit and any controlling-terminal hangup
/// signal (Unix SIGHUP / Windows console-close).
fn spawn_serve_detached() -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("locating current executable")?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Detach from the controlling terminal: setsid() makes the child a
        // new session leader so a closing terminal can't SIGHUP it.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS: child has no console (we already redirect
        // std{in,out,err} to null and the daemon writes to file logs, so
        // no console is needed).
        // CREATE_NEW_PROCESS_GROUP: child becomes its own process-group
        // leader so a Ctrl+C / Ctrl+Break in the parent's console doesn't
        // propagate to it.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    cmd.spawn().context("spawning daemon child")?;
    Ok(())
}

/// Poll `cond` every 100ms until it returns true or `deadline` elapses.
/// Returns `Ok(())` on success, `Err` with a context-friendly message
/// once the deadline trips.
async fn wait_until<F, Fut>(deadline: Duration, mut cond: F) -> anyhow::Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = Instant::now();
    loop {
        if cond().await {
            return Ok(());
        }
        if start.elapsed() >= deadline {
            return Err(anyhow!("timed out waiting for daemon state"));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
