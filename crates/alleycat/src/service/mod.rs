//! Per-OS install / uninstall of an autostart entry for `alleycat serve`.
//!
//! All three platforms are first-class and **never require admin**:
//! - macOS: launchd user agent (`~/Library/LaunchAgents/dev.alleycat.alleycat.plist`).
//! - Linux: systemd user unit, with `~/.config/autostart/alleycat.desktop`
//!   as a fallback for desktops without `systemctl --user`.
//! - Windows: `.lnk` in the per-user Startup folder, written by the `mslnk`
//!   crate (no COM, no admin).

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

/// Reverse-DNS service label, matched by `paths::launchd_plist_path()` and
/// the systemd unit filename. Comes from the [`crate::App`] the binary
/// supplied at startup (e.g. `com.sigkitten.kittylitter` for the shipped
/// kittylitter wrapper, `dev.alleycat.alleycat` for the dev binary).
pub fn service_label() -> &'static str {
    crate::app().label
}

/// Subcommand the autostart entry invokes on the `alleycat` binary.
pub const DAEMON_SUBCOMMAND: &str = "serve";

/// Install the autostart entry. Idempotent — calling twice is a no-op after
/// the file is on disk; the service-manager invocation is re-run so the
/// daemon picks up a binary path change after `cargo install`.
pub fn install() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::install()
    }
    #[cfg(target_os = "linux")]
    {
        linux::install()
    }
    #[cfg(target_os = "windows")]
    {
        windows::install()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Err(anyhow::anyhow!(
            "alleycat install is not supported on this platform"
        ))
    }
}

/// Remove the autostart entry. Idempotent.
pub fn uninstall() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::uninstall()
    }
    #[cfg(target_os = "linux")]
    {
        linux::uninstall()
    }
    #[cfg(target_os = "windows")]
    {
        windows::uninstall()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Ok(())
    }
}

/// True if the autostart file exists on disk. Does not consult the service
/// manager — callers that want "actually running" should ask the daemon over
/// the control socket instead.
pub fn is_installed() -> anyhow::Result<bool> {
    #[cfg(target_os = "macos")]
    {
        Ok(crate::paths::launchd_plist_path()?.exists())
    }
    #[cfg(target_os = "linux")]
    {
        Ok(crate::paths::systemd_unit_path()?.exists()
            || crate::paths::xdg_autostart_path()?.exists())
    }
    #[cfg(target_os = "windows")]
    {
        Ok(crate::paths::windows_startup_lnk_path()?.exists())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Ok(false)
    }
}
