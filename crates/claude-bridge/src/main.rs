//! `alleycat-claude-bridge` binary entry point.
//!
//! Defaults to stdio. With `--socket <path>` (or `ALLEYCAT_BRIDGE_SOCKET`),
//! listens on a Unix socket. The bridge is constructed via
//! [`ClaudeBridgeBuilder`] and served through `bridge_core::serve_*` helpers.

use std::path::PathBuf;

use anyhow::Result;

use alleycat_bridge_core::{ServerOptions, serve_stdio, serve_unix};
use alleycat_claude_bridge::ClaudeBridge;

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
        "alleycat-claude-bridge starting"
    );

    let bridge = ClaudeBridge::builder().from_env().build().await?;

    match socket_arg() {
        Some(path) => {
            serve_unix(
                bridge,
                ServerOptions {
                    socket_path: path,
                    unlink_stale: true,
                },
            )
            .await
        }
        None => serve_stdio(bridge).await,
    }
}

fn socket_arg() -> Option<PathBuf> {
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--socket" || arg == "--listen" {
            return args.next().map(PathBuf::from);
        }
    }
    std::env::var_os("ALLEYCAT_BRIDGE_SOCKET").map(PathBuf::from)
}
