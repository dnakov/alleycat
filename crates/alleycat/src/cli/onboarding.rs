//! Default flow when the binary is invoked with no subcommand. Designed for
//! `npx kittylitter` first-run UX:
//!
//! 1. Install the OS-level autostart entry if it's missing. On macOS/Linux
//!    this also starts the daemon (launchctl bootstrap+kickstart / systemctl
//!    --user start), so by the time `install()` returns the control socket
//!    is up — or about to be.
//! 2. Wait briefly for the control socket to accept connections.
//! 3. Print the pair payload with a QR code so the user can pair their phone
//!    immediately.

use std::time::{Duration, Instant};

use anyhow::anyhow;

use crate::cli;
use crate::ipc;
use crate::service;

pub async fn run() -> anyhow::Result<()> {
    let name = crate::binary_name();

    if !service::is_installed().unwrap_or(false) {
        println!("First run — installing {name} as a user-level autostart...");
        service::install()?;
        println!("installed.");
    }

    // Autostart usually has the daemon listening within a few hundred ms.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if ipc::is_daemon_running().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    if !ipc::is_daemon_running().await {
        return Err(anyhow!(
            "daemon did not come up within 10s. try `{name} status` for details."
        ));
    }

    cli::pair::run(cli::pair::PairArgs { qr: true }).await
}
