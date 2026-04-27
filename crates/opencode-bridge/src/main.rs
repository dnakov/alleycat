use std::sync::Arc;

use alleycat_bridge_core::{ServerOptions, serve_unix};
use alleycat_opencode_bridge::OpencodeBridge;
use alleycat_opencode_bridge::opencode_proc::OpencodeRuntime;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let socket = std::env::var_os("ALLEYCAT_BRIDGE_SOCKET")
        .or_else(|| {
            let mut args = std::env::args_os().skip(1);
            while let Some(arg) = args.next() {
                if arg == "--socket" {
                    return args.next();
                }
            }
            None
        })
        .ok_or_else(|| anyhow::anyhow!("missing --socket or ALLEYCAT_BRIDGE_SOCKET"))?;

    let runtime = OpencodeRuntime::start_from_env().await?;
    let bridge = Arc::new(OpencodeBridge::new(runtime).await?);
    serve_unix(
        bridge,
        ServerOptions {
            socket_path: socket.into(),
            unlink_stale: true,
        },
    )
    .await
}
