use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
pub struct ShellSize {
    pub cols: u16,
    pub rows: u16,
}

impl ShellSize {
    pub fn validate(self) -> anyhow::Result<Self> {
        if self.cols == 0 || self.rows == 0 {
            anyhow::bail!(
                "shell size must be non-zero, got {}x{}",
                self.cols,
                self.rows
            );
        }
        Ok(self)
    }
}

impl From<ShellSize> for PtySize {
    fn from(value: ShellSize) -> Self {
        Self {
            rows: value.rows,
            cols: value.cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

pub struct ShellSession {
    id: String,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
}

pub struct SpawnedShellSession {
    pub session: Arc<ShellSession>,
    pub reader: Box<dyn Read + Send>,
    pub child: Box<dyn Child + Send + Sync>,
}

impl ShellSession {
    pub fn spawn(
        id: String,
        shell: String,
        args: Vec<String>,
        cwd: Option<PathBuf>,
        env: HashMap<String, String>,
        size: ShellSize,
    ) -> anyhow::Result<SpawnedShellSession> {
        let size = size.validate()?;
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(size.into())
            .context("opening PTY pair")?;

        let mut cmd = CommandBuilder::new(&shell);
        cmd.args(args);
        if let Some(cwd) = cwd {
            cmd.cwd(cwd);
        }
        for (key, value) in env {
            cmd.env(key, value);
        }

        let reader = pair
            .master
            .try_clone_reader()
            .context("cloning PTY reader")?;
        let writer = pair.master.take_writer().context("taking PTY writer")?;
        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawning shell `{shell}`"))?;
        let killer = child.clone_killer();

        let session = Arc::new(Self {
            id,
            writer: Mutex::new(writer),
            master: Mutex::new(pair.master),
            killer: Mutex::new(killer),
        });
        Ok(SpawnedShellSession {
            session,
            reader,
            child,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn write(&self, data: &[u8]) -> anyhow::Result<()> {
        let mut writer = self.writer.lock().expect("shell writer mutex poisoned");
        writer.write_all(data).context("writing PTY input")?;
        writer.flush().context("flushing PTY input")
    }

    pub fn resize(&self, size: ShellSize) -> anyhow::Result<()> {
        let size = size.validate()?;
        let master = self.master.lock().expect("shell master mutex poisoned");
        master.resize(size.into()).context("resizing PTY")
    }

    pub fn kill(&self) -> anyhow::Result<()> {
        let mut killer = self.killer.lock().expect("shell killer mutex poisoned");
        killer.kill().context("killing shell child")
    }
}
