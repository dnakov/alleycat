//! Process launcher seam.
//!
//! Bridges spawn agent processes (`pi-coding-agent`, `claude`, `opencode`) and,
//! in the case of pi/claude, also spawn shell-tool subprocesses via the
//! `command_exec` codex protocol. Today every spawn site builds a
//! `tokio::process::Command` directly. To let downstream consumers (the daemon,
//! Litter) substitute a remote launcher (e.g. SSH) without touching bridge
//! internals, every spawn is routed through the [`ProcessLauncher`] trait
//! defined here.
//!
//! [`LocalLauncher`] is the default implementation and preserves the daemon's
//! current behavior bit-for-bit: it wraps `tokio::process::Command` with
//! `kill_on_drop(true)` so a dropped child doesn't outlive the bridge.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};

use futures::future::BoxFuture;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::Command;

/// Why a process is being launched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessRole {
    /// Long-lived coding-agent process owned by a bridge pool.
    Agent,
    /// Short-lived shell/tool command spawned by `command_exec`.
    ToolCommand,
}

/// Stdio configuration for one of `stdin` / `stdout` / `stderr`.
///
/// Mirrors the subset of `std::process::Stdio` bridges actually use. `Inherit`
/// is only meaningful for `stderr` — the bridges keep stdin/stdout piped so
/// they can speak JSON-RPC over them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdioMode {
    Piped,
    Null,
    Inherit,
}

impl StdioMode {
    fn to_std(self) -> Stdio {
        match self {
            StdioMode::Piped => Stdio::piped(),
            StdioMode::Null => Stdio::null(),
            StdioMode::Inherit => Stdio::inherit(),
        }
    }
}

/// Specification of a process to launch. Bridges populate this and hand it to
/// a [`ProcessLauncher`] without caring whether the resulting child runs
/// locally, on a remote host, or in a sandbox.
#[derive(Debug, Clone)]
pub struct ProcessSpec {
    pub role: ProcessRole,
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    /// Environment variables to set on the child. These are layered on top of
    /// the launcher's default environment; each entry overrides any inherited
    /// value with the same key.
    pub env: Vec<(OsString, OsString)>,
    pub stdin: StdioMode,
    pub stdout: StdioMode,
    pub stderr: StdioMode,
}

impl ProcessSpec {
    /// Build a spec with all stdio piped and no extra env.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            role: ProcessRole::Agent,
            stdin: StdioMode::Piped,
            stdout: StdioMode::Piped,
            stderr: StdioMode::Piped,
        }
    }
}

/// Type-erased writer over the child's stdin pipe.
pub type ChildStdin = Box<dyn AsyncWrite + Send + Unpin>;
/// Type-erased reader over the child's stdout pipe.
pub type ChildStdout = Box<dyn AsyncRead + Send + Unpin>;
/// Type-erased reader over the child's stderr pipe.
pub type ChildStderr = Box<dyn AsyncRead + Send + Unpin>;

/// Handle to a launched child process.
///
/// Methods that take `&mut self` (e.g. `wait`, `kill`) match `tokio::process::Child`
/// so the local impl is a thin pass-through. The `take_*` methods consume the
/// pipe — calling them twice returns `None` the second time, exactly like
/// `tokio::process::Child`.
pub trait ChildProcess: Send + Sync {
    fn take_stdin(&mut self) -> Option<ChildStdin>;
    fn take_stdout(&mut self) -> Option<ChildStdout>;
    fn take_stderr(&mut self) -> Option<ChildStderr>;
    /// OS process id, when one is meaningful (`None` for remote launchers
    /// that don't expose one).
    fn id(&self) -> Option<u32>;
    fn wait(&mut self) -> BoxFuture<'_, std::io::Result<ExitStatus>>;
    fn kill(&mut self) -> BoxFuture<'_, std::io::Result<()>>;
}

/// Launch handle. Implementations are typically `Arc`-wrapped (`Arc<dyn
/// ProcessLauncher>`) and shared across the bridge.
pub trait ProcessLauncher: Send + Sync {
    fn launch(&self, spec: ProcessSpec) -> BoxFuture<'_, std::io::Result<Box<dyn ChildProcess>>>;
}

/// Default launcher: forks a local process via `tokio::process::Command` with
/// `kill_on_drop(true)` so children don't outlive the bridge that owns them.
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalLauncher;

impl LocalLauncher {
    pub fn new() -> Self {
        Self
    }
}

impl ProcessLauncher for LocalLauncher {
    fn launch(&self, spec: ProcessSpec) -> BoxFuture<'_, std::io::Result<Box<dyn ChildProcess>>> {
        Box::pin(async move {
            let mut cmd = Command::new(&spec.program);
            cmd.args(&spec.args);
            if let Some(cwd) = &spec.cwd {
                cmd.current_dir(cwd);
            }
            for (k, v) in &spec.env {
                cmd.env(k, v);
            }
            cmd.stdin(spec.stdin.to_std());
            cmd.stdout(spec.stdout.to_std());
            cmd.stderr(spec.stderr.to_std());
            cmd.kill_on_drop(true);
            let child = cmd.spawn()?;
            Ok(Box::new(LocalChild { inner: child }) as Box<dyn ChildProcess>)
        })
    }
}

struct LocalChild {
    inner: tokio::process::Child,
}

impl ChildProcess for LocalChild {
    fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.inner.stdin.take().map(|s| Box::new(s) as ChildStdin)
    }

    fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.inner.stdout.take().map(|s| Box::new(s) as ChildStdout)
    }

    fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.inner.stderr.take().map(|s| Box::new(s) as ChildStderr)
    }

    fn id(&self) -> Option<u32> {
        self.inner.id()
    }

    fn wait(&mut self) -> BoxFuture<'_, std::io::Result<ExitStatus>> {
        Box::pin(async move { self.inner.wait().await })
    }

    fn kill(&mut self) -> BoxFuture<'_, std::io::Result<()>> {
        Box::pin(async move { self.inner.kill().await })
    }
}
