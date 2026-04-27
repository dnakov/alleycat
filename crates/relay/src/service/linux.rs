//! systemd user-unit install for Linux, with XDG-autostart `.desktop`
//! fallback when `systemctl --user` is unavailable.
//!
//! No sudo. We print a hint about `loginctl enable-linger $USER` for users
//! who want daemon-at-boot, but never invoke it ourselves.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, anyhow};

use crate::paths;

pub(super) fn install() -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("resolving current executable")?;

    if has_systemd_user() {
        let unit_path = paths::systemd_unit_path()?;
        write_systemd_unit(&unit_path, &exe)?;
        run_systemctl(&["--user", "daemon-reload"])?;
        run_systemctl(&["--user", "enable", "--now", "alleycat"])?;
        eprintln!(
            "Hint: to start the daemon at boot rather than at login, run:\n  \
             loginctl enable-linger $USER\n\
             (this needs sudo and is intentionally not run by `alleycat install`)"
        );
        return Ok(());
    }

    if has_xdg_session() {
        let desktop_path = paths::xdg_autostart_path()?;
        write_autostart_desktop(&desktop_path, &exe)?;
        eprintln!(
            "Installed XDG autostart entry at {}; the daemon will launch at next graphical login.",
            desktop_path.display()
        );
        return Ok(());
    }

    Err(anyhow!(
        "Linux init not supported (no `systemctl --user`, no XDG graphical session). \
         Run `alleycat run` manually under your init."
    ))
}

pub(super) fn uninstall() -> anyhow::Result<()> {
    if has_systemd_user() {
        let _ = run_systemctl(&["--user", "disable", "--now", "alleycat"]);
    }
    let unit_path = paths::systemd_unit_path()?;
    if unit_path.exists() {
        std::fs::remove_file(&unit_path)
            .with_context(|| format!("removing {}", unit_path.display()))?;
    }
    let desktop_path = paths::xdg_autostart_path()?;
    if desktop_path.exists() {
        std::fs::remove_file(&desktop_path)
            .with_context(|| format!("removing {}", desktop_path.display()))?;
    }
    if has_systemd_user() {
        let _ = run_systemctl(&["--user", "daemon-reload"]);
    }
    Ok(())
}

/// Write the systemd user unit atomically. Pure file I/O — no `systemctl`.
pub(super) fn write_systemd_unit(unit_path: &Path, exe: &Path) -> anyhow::Result<()> {
    if let Some(parent) = unit_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let body = render_systemd_unit(exe);
    let tmp = unit_path.with_extension("service.tmp");
    std::fs::write(&tmp, body.as_bytes()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, unit_path)
        .with_context(|| format!("renaming into {}", unit_path.display()))?;
    Ok(())
}

/// Write the `.desktop` autostart file atomically. Pure file I/O.
pub(super) fn write_autostart_desktop(desktop_path: &Path, exe: &Path) -> anyhow::Result<()> {
    if let Some(parent) = desktop_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let body = render_autostart_desktop(exe);
    let tmp = desktop_path.with_extension("desktop.tmp");
    std::fs::write(&tmp, body.as_bytes()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, desktop_path)
        .with_context(|| format!("renaming into {}", desktop_path.display()))?;
    Ok(())
}

fn render_systemd_unit(exe: &Path) -> String {
    let exe = exe.to_string_lossy();
    format!(
        "[Unit]\n\
         Description=Alleycat relay daemon\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} run\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

fn render_autostart_desktop(exe: &Path) -> String {
    let exe = exe.to_string_lossy();
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Alleycat\n\
         Exec={exe} run\n\
         Hidden=false\n\
         X-GNOME-Autostart-enabled=true\n\
         NoDisplay=true\n"
    )
}

fn has_systemd_user() -> bool {
    Command::new("systemctl")
        .args(["--user", "--version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn has_xdg_session() -> bool {
    std::env::var_os("XDG_CURRENT_DESKTOP")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
        || std::env::var_os("XDG_SESSION_TYPE")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
}

fn run_systemctl(args: &[&str]) -> anyhow::Result<()> {
    let status = Command::new("systemctl")
        .args(args)
        .status()
        .with_context(|| format!("running systemctl {}", args.join(" ")))?;
    if !status.success() {
        return Err(anyhow!(
            "systemctl {} failed (exit {:?})",
            args.join(" "),
            status.code()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tempdir() -> PathBuf {
        let mut path = std::env::temp_dir();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        path.push(format!("alleycat-svc-linux-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&path).expect("temp dir");
        path
    }

    #[test]
    fn write_systemd_unit_contains_exec_start() {
        let tmp = tempdir();
        let unit = tmp.join("alleycat.service");
        let exe = PathBuf::from("/opt/alleycat/bin/alleycat");
        write_systemd_unit(&unit, &exe).expect("write unit");
        let body = std::fs::read_to_string(&unit).expect("read unit");
        assert!(body.contains("ExecStart=/opt/alleycat/bin/alleycat run"));
        assert!(body.contains("Restart=on-failure"));
        assert!(body.contains("WantedBy=default.target"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn write_autostart_desktop_contains_exec() {
        let tmp = tempdir();
        let desktop = tmp.join("alleycat.desktop");
        let exe = PathBuf::from("/usr/bin/alleycat");
        write_autostart_desktop(&desktop, &exe).expect("write desktop");
        let body = std::fs::read_to_string(&desktop).expect("read desktop");
        assert!(body.contains("Exec=/usr/bin/alleycat run"));
        assert!(body.contains("[Desktop Entry]"));
        assert!(body.contains("Type=Application"));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
