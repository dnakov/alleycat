//! Daemon logging setup. Writes to a daily-rotated file under
//! [`crate::paths::log_dir`] and, when stderr is a TTY, additionally mirrors
//! to stderr so `alleycat serve` is debuggable from a terminal.
//!
//! The returned [`tracing_appender::non_blocking::WorkerGuard`] must be kept
//! alive for the daemon's lifetime — dropping it stops the background writer
//! and silently swallows pending log lines.

use std::io::IsTerminal;
use std::path::Path;

use anyhow::Context;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Initialize daemon-side tracing.
///
/// `level` is appended to a baseline `iroh=warn,quinn=warn` filter so noisy
/// transport internals stay quiet by default. `RUST_LOG` overrides everything.
pub fn init(level: &str, log_dir: &Path) -> anyhow::Result<WorkerGuard> {
    std::fs::create_dir_all(log_dir)
        .with_context(|| format!("creating log dir {}", log_dir.display()))?;

    let appender = tracing_appender::rolling::daily(log_dir, "daemon.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("{level},iroh=warn,quinn=warn")));

    let file_layer = fmt::layer()
        .with_ansi(false)
        .with_target(true)
        .with_writer(writer);

    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer);

    if std::io::stderr().is_terminal() {
        let stderr_layer = fmt::layer()
            .with_ansi(true)
            .with_target(false)
            .with_writer(std::io::stderr);
        registry.with(stderr_layer).try_init().ok();
    } else {
        registry.try_init().ok();
    }

    Ok(guard)
}
