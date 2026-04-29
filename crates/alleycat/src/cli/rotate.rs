use crate::cli;
use crate::daemon::control::{Request, RotateResult, token_fingerprint};
use crate::ipc;

pub async fn run() -> anyhow::Result<()> {
    if ipc::is_daemon_running().await {
        let resp = cli::send(Request::Rotate).await?;
        let result: RotateResult = cli::decode_data(resp)?;
        println!("rotated. new token (sha256/16): {}", result.token_short);
        return Ok(());
    }
    // Daemon not running — rewrite host.toml directly.
    let cfg = crate::config::rotate_token().await?;
    println!(
        "rotated. new token (sha256/16): {} (daemon not running)",
        token_fingerprint(&cfg.token)
    );
    Ok(())
}
