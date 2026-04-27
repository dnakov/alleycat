use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::Context;
use tokio::net::{UnixListener, UnixStream};

use super::ControlStream;
use crate::paths;

pub(super) struct Listener {
    listener: UnixListener,
    /// Held for cleanup on drop. Best-effort — we don't error if the user
    /// nuked the socket from under us.
    path: PathBuf,
}

impl Listener {
    pub async fn bind() -> anyhow::Result<Self> {
        let path = paths::control_socket_path()?;
        Self::bind_at(path).await
    }

    /// Bind at an explicit path. Pulled out so tests can drive a tempdir
    /// without touching `$HOME`.
    pub async fn bind_at(path: PathBuf) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!("creating control socket parent dir {}", parent.display())
            })?;
        }
        // Stale-socket recovery: if something exists at that path and we
        // can't connect to it, treat it as a corpse and unlink.
        if path.exists() && UnixStream::connect(&path).await.is_err() {
            let _ = std::fs::remove_file(&path);
        }
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("binding unix listener at {}", path.display()))?;
        // Tighten the socket file's mode to owner-only. Best-effort: if
        // the FS doesn't honor unix perms (e.g. NFS without acl), the
        // kernel-side accept is still gated by the parent dir's 0700.
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        Ok(Self { listener, path })
    }

    pub async fn accept(&mut self) -> anyhow::Result<Box<dyn ControlStream>> {
        let (stream, _) = self
            .listener
            .accept()
            .await
            .context("accepting control connection")?;
        Ok(Box::new(stream))
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // Remove the socket file so the next daemon start doesn't have to
        // dance around a stale entry. Ignore errors — the file might be
        // gone already, or live on a read-only fs.
        let _ = std::fs::remove_file(&self.path);
    }
}

pub(super) async fn connect() -> anyhow::Result<Box<dyn ControlStream>> {
    let path = paths::control_socket_path()?;
    connect_at(&path).await
}

pub(super) async fn connect_at(path: &Path) -> anyhow::Result<Box<dyn ControlStream>> {
    let stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("connecting to control socket {}", path.display()))?;
    Ok(Box::new(stream))
}
