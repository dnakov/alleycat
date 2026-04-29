//! `alleycat-pi-bridge` binary entry point.
//!
//! Defaults to stdio. With `--socket <path>` (alias `--listen`) or
//! `ALLEYCAT_BRIDGE_SOCKET=<path>` it listens on a Unix socket. The daemon
//! path (alleycat host) constructs `PiBridge` directly via the builder; this
//! binary is a thin shell around the same builder for standalone test /
//! developer workflows.

use std::path::PathBuf;

use alleycat_bridge_core::ServerOptions;
use alleycat_pi_bridge::PiBridge;
use anyhow::Result;

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

    let bridge = PiBridge::builder().from_env().build().await?;

    match socket_arg() {
        Some(path) => {
            tracing::info!(socket = %path.display(), "pi bridge socket listening");
            alleycat_bridge_core::serve_unix(
                bridge,
                ServerOptions {
                    socket_path: path,
                    unlink_stale: true,
                },
            )
            .await
        }
        None => alleycat_bridge_core::serve_stdio(bridge).await,
    }
}

/// Accept `--socket <path>`, `--listen <path>`, or the `ALLEYCAT_BRIDGE_SOCKET`
/// env var. Two CLI spellings exist so existing scripts using `--socket` keep
/// working.
fn socket_arg() -> Option<PathBuf> {
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--socket" || arg == "--listen" {
            return args.next().map(PathBuf::from);
        }
    }
    std::env::var_os("ALLEYCAT_BRIDGE_SOCKET").map(PathBuf::from)
}
