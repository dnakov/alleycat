//! launchd user-agent install for macOS. Writes a plist under
//! `~/Library/LaunchAgents/` and bootstraps it into `gui/$UID`. No admin.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, anyhow};

use crate::paths;
use crate::service::service_label;

pub(super) fn install() -> anyhow::Result<()> {
    let plist_path = paths::launchd_plist_path()?;
    let exe = std::env::current_exe().context("resolving current executable for launchd plist")?;
    let log_path = paths::log_dir()?.join("daemon.log");

    write_plist(&plist_path, &exe, &log_path)?;

    let uid = current_uid();
    // bootout is a no-op the first time; silence its "no such process" stderr
    // and ignore failure so re-install stays idempotent.
    let _ = Command::new("launchctl")
        .args([
            "bootout",
            &format!("gui/{uid}/{label}", label = service_label()),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    let bootstrap = Command::new("launchctl")
        .args([
            "bootstrap",
            &format!("gui/{uid}"),
            plist_path
                .to_str()
                .ok_or_else(|| anyhow!("plist path is not valid UTF-8"))?,
        ])
        .status()
        .context("running launchctl bootstrap")?;
    if !bootstrap.success() {
        return Err(anyhow!(
            "launchctl bootstrap failed (exit {:?})",
            bootstrap.code()
        ));
    }

    let _ = Command::new("launchctl")
        .args([
            "enable",
            &format!("gui/{uid}/{label}", label = service_label()),
        ])
        .status();

    let _ = Command::new("launchctl")
        .args([
            "kickstart",
            &format!("gui/{uid}/{label}", label = service_label()),
        ])
        .status();

    Ok(())
}

pub(super) fn uninstall() -> anyhow::Result<()> {
    let plist_path = paths::launchd_plist_path()?;
    let uid = current_uid();
    let _ = Command::new("launchctl")
        .args([
            "bootout",
            &format!("gui/{uid}/{label}", label = service_label()),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if plist_path.exists() {
        std::fs::remove_file(&plist_path)
            .with_context(|| format!("removing {}", plist_path.display()))?;
    }
    Ok(())
}

/// Write the launchd plist atomically (`.tmp` + rename). Pure file I/O —
/// callable from tests under a tempdir-overridden HOME without invoking
/// launchctl.
pub(super) fn write_plist(plist_path: &Path, exe: &Path, log_path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let body = render_plist(exe, log_path);
    let tmp = plist_path.with_extension("plist.tmp");
    std::fs::write(&tmp, body.as_bytes()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, plist_path)
        .with_context(|| format!("renaming into {}", plist_path.display()))?;
    Ok(())
}

fn render_plist(exe: &Path, log_path: &Path) -> String {
    let label = service_label();
    let exe = xml_escape(&exe.to_string_lossy());
    let log = xml_escape(&log_path.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTD/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>run</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn current_uid() -> u32 {
    // Safe: getuid is signal-safe and never fails.
    unsafe { libc::getuid() }
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
        path.push(format!("alleycat-svc-macos-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&path).expect("temp dir");
        path
    }

    #[test]
    fn write_plist_renders_expected_keys() {
        let tmp = tempdir();
        let plist = tmp.join("dev.alleycat.alleycat.plist");
        let exe = PathBuf::from("/usr/local/bin/alleycat");
        let log = tmp.join("daemon.log");
        write_plist(&plist, &exe, &log).expect("write_plist");
        let body = std::fs::read_to_string(&plist).expect("read plist");
        assert!(body.contains("<string>dev.alleycat.alleycat</string>"));
        assert!(body.contains("<string>/usr/local/bin/alleycat</string>"));
        assert!(body.contains("<string>run</string>"));
        assert!(body.contains("<key>RunAtLoad</key>"));
        assert!(body.contains("<key>KeepAlive</key>"));
        let log_str = log.to_string_lossy().to_string();
        assert!(
            body.contains(&log_str),
            "expected log path in plist: {log_str}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn xml_escape_handles_specials() {
        assert_eq!(xml_escape("a&b<c>d"), "a&amp;b&lt;c&gt;d");
    }
}
