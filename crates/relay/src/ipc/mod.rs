//! Per-OS control IPC abstraction.
//!
//! On Unix the daemon listens on a Unix domain socket at
//! `paths::control_socket_path()` (mode 0600). On Windows it listens on a
//! per-user named pipe at `paths::control_pipe_name()`. This module hides
//! both behind a common [`ControlListener`] / [`ControlStream`] surface so
//! callers (the daemon accept loop, the CLI dispatch) never see the
//! platform difference.
//!
//! Wire framing is NOT done here — callers stack
//! `alleycat_protocol::frame::{read_frame_json, write_frame_json}` on top of
//! the returned `ControlStream`.

use tokio::io::{AsyncRead, AsyncWrite};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

/// Marker trait for an established control-socket connection. Any
/// `AsyncRead + AsyncWrite + Unpin + Send + 'static` qualifies.
pub trait ControlStream: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> ControlStream for T {}

/// A per-platform listener that yields `ControlStream`s.
pub struct ControlListener {
    #[cfg(unix)]
    inner: unix::Listener,
    #[cfg(windows)]
    inner: windows::Listener,
}

impl ControlListener {
    /// Bind the daemon-side listener at the OS-appropriate location.
    /// Unlinks a stale Unix socket if the previous owner left it behind.
    pub async fn bind() -> anyhow::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                inner: unix::Listener::bind().await?,
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                inner: windows::Listener::bind().await?,
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            anyhow::bail!("control IPC is not supported on this platform")
        }
    }

    /// Accept the next inbound control connection. Returns a boxed stream so
    /// the caller can hand it to a generic frame reader/writer without
    /// caring which platform produced it.
    pub async fn accept(&mut self) -> anyhow::Result<Box<dyn ControlStream>> {
        self.inner.accept().await
    }
}

/// Dial the daemon. Used by the CLI to issue control requests.
pub async fn connect() -> anyhow::Result<Box<dyn ControlStream>> {
    #[cfg(unix)]
    {
        unix::connect().await
    }
    #[cfg(windows)]
    {
        windows::connect().await
    }
    #[cfg(not(any(unix, windows)))]
    {
        anyhow::bail!("control IPC is not supported on this platform")
    }
}

/// Cheap probe: is something currently listening on the daemon socket?
/// Returns true on a successful connect, false otherwise. The CLI uses this
/// to decide between "daemon already running, redirect through it" vs
/// "no daemon — we should start one / fail fast".
pub async fn is_daemon_running() -> bool {
    connect().await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alleycat_protocol::{read_frame_json, write_frame_json};
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
    struct Hello {
        msg: String,
        n: u32,
    }

    /// Build a unique tempdir-scoped path so concurrent test runs don't
    /// clobber each other. We deliberately avoid going through
    /// `paths::control_socket_path()` here — that pulls $HOME, which the
    /// `paths` test module already serializes via its own ENV_LOCK and we
    /// don't want to share locks across modules. Tests for the env-driven
    /// path live in `paths::tests`; tests here exercise the IPC mechanics.
    #[cfg(unix)]
    fn unique_socket_path(label: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        p.push(format!(
            "alleycat-ipc-{label}-{}-{}.sock",
            std::process::id(),
            stamp
        ));
        p
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_round_trip_json_frame() {
        let path = unique_socket_path("rt");
        let mut listener = unix::Listener::bind_at(path.clone())
            .await
            .expect("bind listener");

        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept");
            let req: Hello = read_frame_json(&mut stream).await.expect("read");
            let resp = Hello {
                msg: format!("echo: {}", req.msg),
                n: req.n + 1,
            };
            write_frame_json(&mut stream, &resp).await.expect("write");
        });

        let mut client = unix::connect_at(&path).await.expect("connect");
        let req = Hello {
            msg: "ping".into(),
            n: 41,
        };
        write_frame_json(&mut client, &req).await.expect("send");
        let resp: Hello = read_frame_json(&mut client).await.expect("recv");
        assert_eq!(resp.msg, "echo: ping");
        assert_eq!(resp.n, 42);

        server.await.expect("server task");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_socket_has_owner_only_perms() {
        use std::os::unix::fs::PermissionsExt;
        let path = unique_socket_path("perms");
        let _listener = unix::Listener::bind_at(path.clone())
            .await
            .expect("bind listener");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "control socket must be 0600, got {mode:o}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_unlinks_stale_socket_file() {
        use std::os::unix::fs::PermissionsExt;
        let path = unique_socket_path("stale");
        // Plant a stale socket file (regular file masquerading as the
        // socket — exactly the kind of thing left after a kill -9).
        std::fs::write(&path, b"stale").unwrap();

        let _listener = unix::Listener::bind_at(path.clone())
            .await
            .expect("bind over stale");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_connect_at_missing_socket_errors() {
        let path = unique_socket_path("missing");
        assert!(unix::connect_at(&path).await.is_err());
    }
}
