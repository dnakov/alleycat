//! `alleycat-pi-bridge` binary entry point.
//!
//! Defaults to stdio for the existing test/developer workflow. When launched
//! with `--socket <path>` (alias `--listen <path>`) or
//! `ALLEYCAT_BRIDGE_SOCKET=<path>`, it listens on a Unix socket for legacy
//! developer workflows. The iroh-backed `alleycat` host path calls `run_connection`
//! directly on each authenticated stream, reusing one shared `PiPool` +
//! `ThreadIndex`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use tokio::net::UnixListener;

use alleycat_pi_bridge::handlers;
use alleycat_pi_bridge::index::ThreadIndex;
use alleycat_pi_bridge::pool::PiPool;
use alleycat_pi_bridge::run_connection;
use alleycat_pi_bridge::state::ThreadIndexHandle;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "alleycat-pi-bridge starting"
    );

    let codex_home = handlers::lifecycle::default_codex_home();
    if let Err(err) = std::fs::create_dir_all(&codex_home) {
        tracing::warn!(?codex_home, %err, "failed to ensure codex_home; continuing");
    }

    let pi_pool = Arc::new(PiPool::new(resolve_pi_bin()));
    let thread_index: Arc<dyn ThreadIndexHandle> = ThreadIndex::open_and_hydrate(&codex_home)
        .await
        .context("opening pi-bridge thread index")?;

    match socket_arg() {
        Some(path) => serve_socket(pi_pool, thread_index, codex_home, path).await,
        None => {
            // Stdio mode: one connection, lives for the lifetime of the
            // parent, usually a developer test client. Reuses
            // `run_connection` so the dispatch surface stays single-sourced
            // with the alleycat host path.
            let stdin = tokio::io::stdin();
            let stdout = tokio::io::stdout();
            run_connection(stdin, stdout, pi_pool, thread_index, codex_home).await
        }
    }
}

/// Accept `--socket <path>`, the spec'd `--listen <path>` alias, or the
/// `ALLEYCAT_BRIDGE_SOCKET` env var. The two CLI spellings exist so the
/// existing scripts using `--socket` keep working.
fn socket_arg() -> Option<PathBuf> {
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--socket" || arg == "--listen" {
            return args.next().map(PathBuf::from);
        }
    }
    std::env::var_os("ALLEYCAT_BRIDGE_SOCKET").map(PathBuf::from)
}

fn resolve_pi_bin() -> PathBuf {
    std::env::var_os("PI_BRIDGE_PI_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("pi-coding-agent"))
}

async fn serve_socket(
    pi_pool: Arc<PiPool>,
    thread_index: Arc<dyn ThreadIndexHandle>,
    codex_home: PathBuf,
    path: PathBuf,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    // Stale socket from a previous crash would block bind; remove any leftover
    // file before we try. NotFound is fine — that just means a fresh start.
    match tokio::fs::remove_file(&path).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener =
        UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    tracing::info!(socket = %path.display(), "pi bridge socket listening");

    loop {
        let (stream, _) = listener.accept().await?;
        let pi_pool = Arc::clone(&pi_pool);
        let thread_index = Arc::clone(&thread_index);
        let codex_home = codex_home.clone();
        tokio::spawn(async move {
            let (reader, writer) = tokio::io::split(stream);
            if let Err(error) =
                run_connection(reader, writer, pi_pool, thread_index, codex_home).await
            {
                tracing::debug!("pi bridge socket connection ended: {error:#}");
            }
        });
    }
}
