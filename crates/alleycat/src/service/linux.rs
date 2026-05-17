//! systemd user-unit install for Linux, with XDG-autostart `.desktop`
//! fallback when the systemd user manager is not reachable.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, anyhow};

use crate::paths;
use crate::service::DAEMON_SUBCOMMAND;

pub(super) fn install() -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("resolving current executable")?;
    let inherit_path = std::env::var("PATH").ok();
    let inherit_shell = std::env::var("SHELL").ok();

    if systemd_user_session_available() {
        let unit_path = paths::systemd_unit_path()?;
        let unit_name = systemd_unit_name(&unit_path)?;
        write_systemd_unit(
            &unit_path,
            &exe,
            inherit_path.as_deref(),
            inherit_shell.as_deref(),
        )?;
        run_systemctl(&["--user", "daemon-reload"])?;
        run_systemctl(&["--user", "enable", "--now", &unit_name])?;
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
        "Linux init not supported (no reachable `systemd --user` session, no XDG graphical session). \
         Run `alleycat serve` manually under your init."
    ))
}

pub(super) fn uninstall() -> anyhow::Result<()> {
    let unit_path = paths::systemd_unit_path()?;
    if systemd_user_session_available() {
        if let Ok(unit_name) = systemd_unit_name(&unit_path) {
            let _ = run_systemctl(&["--user", "disable", "--now", &unit_name]);
        }
    }
    if unit_path.exists() {
        std::fs::remove_file(&unit_path)
            .with_context(|| format!("removing {}", unit_path.display()))?;
    }
    let desktop_path = paths::xdg_autostart_path()?;
    if desktop_path.exists() {
        std::fs::remove_file(&desktop_path)
            .with_context(|| format!("removing {}", desktop_path.display()))?;
    }
    if systemd_user_session_available() {
        let _ = run_systemctl(&["--user", "daemon-reload"]);
    }
    Ok(())
}

pub(super) fn is_installed() -> anyhow::Result<bool> {
    let unit_path = paths::systemd_unit_path()?;
    if unit_path.exists() {
        if systemd_user_session_available() {
            let unit_name = systemd_unit_name(&unit_path)?;
            return Ok(systemd_unit_is_enabled(&unit_name));
        }
        return Ok(true);
    }
    Ok(paths::xdg_autostart_path()?.exists())
}

pub(super) fn write_systemd_unit(
    unit_path: &Path,
    exe: &Path,
    inherit_path: Option<&str>,
    inherit_shell: Option<&str>,
) -> anyhow::Result<()> {
    if let Some(parent) = unit_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let body = render_systemd_unit(exe, inherit_path, inherit_shell);
    let tmp = unit_path.with_extension("service.tmp");
    std::fs::write(&tmp, body.as_bytes()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, unit_path)
        .with_context(|| format!("renaming into {}", unit_path.display()))?;
    Ok(())
}

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

fn render_systemd_unit(
    exe: &Path,
    inherit_path: Option<&str>,
    inherit_shell: Option<&str>,
) -> String {
    let exe = exe.to_string_lossy();
    // systemd `--user` units inherit `DefaultEnvironment=` from `manager.conf`,
    // not the user's interactive shell. Propagating PATH preserves the
    // expectation that `which("opencode")` / `which("pi")` resolves the
    // same way it does in the shell that ran `alleycat install`. SHELL is safe
    // to persist and lets the launch-environment resolver choose fish, zsh,
    // bash, or sh the same way the user does.
    let mut env_line = String::new();
    if let Some(path) = inherit_path {
        env_line.push_str(&format!("Environment=\"PATH={}\"\n", systemd_escape(path)));
    }
    if let Some(shell) = inherit_shell {
        env_line.push_str(&format!(
            "Environment=\"SHELL={}\"\n",
            systemd_escape(shell)
        ));
    }
    format!(
        "[Unit]\n\
         Description=Alleycat bridge daemon\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} {DAEMON_SUBCOMMAND}\n\
         {env_line}\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

fn systemd_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn render_autostart_desktop(exe: &Path) -> String {
    let exe = exe.to_string_lossy();
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Alleycat\n\
         Exec={exe} {DAEMON_SUBCOMMAND}\n\
         Hidden=false\n\
         X-GNOME-Autostart-enabled=true\n\
         NoDisplay=true\n"
    )
}

fn systemd_user_session_available() -> bool {
    Command::new("systemctl")
        .args(["--user", "show-environment"])
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

fn systemd_unit_name(unit_path: &Path) -> anyhow::Result<String> {
    unit_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("invalid systemd unit path {}", unit_path.display()))
}

fn systemd_unit_is_enabled(unit_name: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", "is-enabled", unit_name])
        .output()
        .map(|o| o.status.success())
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
    use crate::test_support::TempHome;
    use std::os::unix::fs::PermissionsExt;
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

    fn write_fake_systemctl(dir: &Path, log: &Path) -> PathBuf {
        let script = dir.join("systemctl");
        let body = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> \"{}\"\n\
             case \"$*\" in\n\
               \"--user show-environment\")\n\
                 exit 1\n\
                 ;;\n\
               \"--user --version\")\n\
                 printf 'systemd 999\\n'\n\
                 exit 0\n\
                 ;;\n\
               *)\n\
                 exit 0\n\
                 ;;\n\
             esac\n",
            log.display()
        );
        std::fs::write(&script, body).expect("write fake systemctl");
        let mut perms = std::fs::metadata(&script)
            .expect("fake systemctl metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).expect("make fake systemctl executable");
        script
    }

    #[test]
    fn write_systemd_unit_contains_exec_start() {
        let tmp = tempdir();
        let unit = tmp.join("alleycat.service");
        let exe = PathBuf::from("/opt/alleycat/bin/alleycat");
        write_systemd_unit(&unit, &exe, None, None).expect("write unit");
        let body = std::fs::read_to_string(&unit).expect("read unit");
        assert!(body.contains(&format!(
            "ExecStart=/opt/alleycat/bin/alleycat {DAEMON_SUBCOMMAND}"
        )));
        assert!(body.contains("Restart=on-failure"));
        assert!(body.contains("WantedBy=default.target"));
        assert!(
            !body.contains("Environment=\"PATH="),
            "no inherit_path → no Environment line"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn write_systemd_unit_includes_environment_when_inherit_path_set() {
        let tmp = tempdir();
        let unit = tmp.join("alleycat.service");
        let exe = PathBuf::from("/opt/alleycat/bin/alleycat");
        write_systemd_unit(
            &unit,
            &exe,
            Some("/home/me/.local/bin:/usr/local/bin:/usr/bin"),
            Some("/usr/bin/fish"),
        )
        .expect("write unit");
        let body = std::fs::read_to_string(&unit).expect("read unit");
        assert!(body.contains("Environment=\"PATH=/home/me/.local/bin:/usr/local/bin:/usr/bin\""));
        assert!(body.contains("Environment=\"SHELL=/usr/bin/fish\""));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn write_autostart_desktop_contains_exec() {
        let tmp = tempdir();
        let desktop = tmp.join("alleycat.desktop");
        let exe = PathBuf::from("/usr/bin/alleycat");
        write_autostart_desktop(&desktop, &exe).expect("write desktop");
        let body = std::fs::read_to_string(&desktop).expect("read desktop");
        assert!(body.contains(&format!("Exec=/usr/bin/alleycat {DAEMON_SUBCOMMAND}")));
        assert!(body.contains("[Desktop Entry]"));
        assert!(body.contains("Type=Application"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn systemd_unit_name_uses_actual_packaged_unit_filename() {
        let unit = PathBuf::from("/home/me/.config/systemd/user/kittylitter.service");
        assert_eq!(
            systemd_unit_name(&unit).expect("unit name"),
            "kittylitter.service"
        );
    }

    #[test]
    fn install_rejects_headless_sessions_without_touching_systemd() {
        let mut env = TempHome::new();
        let tmp = tempdir();
        let log = tmp.join("systemctl.log");
        let fake_bin = tmp.join("bin");
        std::fs::create_dir_all(&fake_bin).expect("fake bin dir");
        let _systemctl = write_fake_systemctl(&fake_bin, &log);

        let path = std::env::join_paths([fake_bin.as_os_str()]).expect("join PATH");
        env.override_env(&[
            ("PATH", path.to_str().expect("PATH utf-8")),
            ("XDG_CURRENT_DESKTOP", ""),
            ("XDG_SESSION_TYPE", ""),
        ]);

        let err = install().expect_err("headless install should fail cleanly");
        let msg = err.to_string();
        assert!(
            msg.contains("Linux init not supported"),
            "unexpected error: {msg}"
        );

        let log_body = std::fs::read_to_string(&log).expect("read fake systemctl log");
        assert!(
            log_body.contains("--user show-environment"),
            "probe should check for a reachable user bus"
        );
        assert!(
            !log_body.contains("daemon-reload"),
            "install must not try to reload user units when the bus is unavailable"
        );
    }

    #[test]
    fn is_installed_treats_existing_systemd_unit_as_installed_without_bus() {
        let mut home = TempHome::new();
        let tmp = tempdir();
        let log = tmp.join("systemctl.log");
        let fake_bin = tmp.join("bin");
        std::fs::create_dir_all(&fake_bin).expect("fake bin dir");
        let _systemctl = write_fake_systemctl(&fake_bin, &log);

        let path = std::env::join_paths([fake_bin.as_os_str()]).expect("join PATH");
        home.override_env(&[
            ("PATH", path.to_str().expect("PATH utf-8")),
            ("XDG_CURRENT_DESKTOP", ""),
            ("XDG_SESSION_TYPE", ""),
        ]);

        let unit_path = paths::systemd_unit_path().expect("systemd unit path");
        if let Some(parent) = unit_path.parent() {
            std::fs::create_dir_all(parent).expect("create unit parent");
        }
        std::fs::write(&unit_path, b"[Unit]\nDescription=Alleycat\n").expect("write unit");

        assert!(
            is_installed().expect("check installed"),
            "unit on disk should count as installed"
        );
    }
}
