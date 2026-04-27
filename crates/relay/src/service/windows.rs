//! Windows Startup-folder install. Writes a `.lnk` to
//! `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\alleycat.lnk`
//! using the `mslnk` crate (pure-Rust, no COM, no admin).
//!
//! `ShowMinNoActive` keeps the daemon's console window from flashing on
//! login.

use std::path::Path;

use anyhow::{Context, anyhow};
use mslnk::{ShellLink, ShowCommand};

use crate::paths;

pub(super) fn install() -> anyhow::Result<()> {
    let lnk_path = paths::windows_startup_lnk_path()?;
    let exe = std::env::current_exe().context("resolving current executable")?;
    write_startup_lnk(&lnk_path, &exe)
}

pub(super) fn uninstall() -> anyhow::Result<()> {
    let lnk_path = paths::windows_startup_lnk_path()?;
    if lnk_path.exists() {
        std::fs::remove_file(&lnk_path)
            .with_context(|| format!("removing {}", lnk_path.display()))?;
    }
    Ok(())
}

/// Pure-data .lnk write. Callable from tests without touching the real
/// Startup folder.
pub(super) fn write_startup_lnk(lnk_path: &Path, exe: &Path) -> anyhow::Result<()> {
    if let Some(parent) = lnk_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let exe_str = exe
        .to_str()
        .ok_or_else(|| anyhow!("executable path is not valid UTF-8: {}", exe.display()))?;
    let mut lnk =
        ShellLink::new(exe_str).with_context(|| format!("creating ShellLink for {exe_str}"))?;
    lnk.header_mut()
        .set_show_command(ShowCommand::ShowMinNoActive);
    lnk.set_arguments(Some("run".to_string()));
    lnk.create_lnk(lnk_path)
        .with_context(|| format!("writing {}", lnk_path.display()))?;
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
        path.push(format!(
            "alleycat-svc-windows-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("temp dir");
        path
    }

    #[test]
    fn write_startup_lnk_creates_file() {
        let tmp = tempdir();
        let lnk = tmp.join("alleycat.lnk");
        // mslnk verifies the target exists in `ShellLink::new`. Use the
        // current test binary as the target so the call succeeds.
        let exe = std::env::current_exe().expect("current_exe");
        write_startup_lnk(&lnk, &exe).expect("write_startup_lnk");
        assert!(lnk.exists(), "lnk should be on disk");
        let bytes = std::fs::read(&lnk).expect("read lnk");
        assert!(!bytes.is_empty(), "lnk should not be empty");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
