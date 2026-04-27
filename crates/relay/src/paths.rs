//! Cross-platform filesystem path helpers for the alleycat daemon.
//!
//! Wraps `directories::ProjectDirs` so callers never see `Option<PathBuf>`
//! ambiguity. Per-OS layout is documented in the project plan; the short
//! version is:
//!
//! | concern    | macOS                                  | Linux                       | Windows                                  |
//! | ---------- | -------------------------------------- | --------------------------- | ---------------------------------------- |
//! | config     | ~/Library/Application Support/<id>/    | $XDG_CONFIG_HOME/alleycat/  | %APPDATA%\Alleycat\alleycat\config\      |
//! | state      | (collapses to config_dir)              | $XDG_STATE_HOME/alleycat/   | %LOCALAPPDATA%\Alleycat\alleycat\data\   |
//! | logs       | ~/Library/Logs/<id>/                   | <state>/logs/               | <data_local>/logs/                       |
//! | control IPC| $TMPDIR/alleycat-<hash>/control.sock   | $XDG_RUNTIME_DIR or <state> | \\.\pipe\alleycat-control-<userhash>     |
//!
//! On Unix the control socket intentionally does **not** live under
//! `state_dir()` — `sockaddr_un.sun_path` is capped at 104 bytes on macOS
//! / BSD (108 on Linux), and the natural state-dir paths blow that limit
//! under hermetic test homes (e.g. `/var/folders/.../T/...`). See
//! `control_socket_path` for the candidate-resolution order.

use std::path::PathBuf;

use anyhow::{Context, anyhow};
use directories::{BaseDirs, ProjectDirs};

const QUALIFIER: &str = "dev";
const ORGANIZATION: &str = "Alleycat";
const APPLICATION: &str = "alleycat";

/// macOS bundle-style identifier derived from the qualifier/org/app triple
/// above. Used for log directory + launchd plist filename.
const PROJECT_ID: &str = "dev.Alleycat.alleycat";

/// Reverse-DNS label used for the launchd Label and plist filename.
/// Lower-cased on purpose — matches Apple convention for user agents.
const LAUNCHD_LABEL: &str = "dev.alleycat.alleycat";

fn project_dirs() -> anyhow::Result<ProjectDirs> {
    ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)
        .ok_or_else(|| anyhow!("could not determine project directories (no $HOME?)"))
}

fn base_dirs() -> anyhow::Result<BaseDirs> {
    BaseDirs::new().ok_or_else(|| anyhow!("could not determine base directories (no $HOME?)"))
}

fn ensure_dir(path: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("creating directory {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        // Best-effort tighten — ignore EPERM on existing dirs the user has
        // already locked down differently.
        let _ = std::fs::set_permissions(path, perms);
    }
    Ok(())
}

/// Per-user config directory. Created with mode 0700 on Unix.
pub fn config_dir() -> anyhow::Result<PathBuf> {
    let dirs = project_dirs()?;
    let path = dirs.config_dir().to_path_buf();
    ensure_dir(&path)?;
    Ok(path)
}

/// Per-user state directory (cert/key/token/port/lock live here).
///
/// - Linux: `$XDG_STATE_HOME/alleycat` (e.g. `~/.local/state/alleycat`)
/// - macOS: collapses to `config_dir()` since `ProjectDirs::state_dir` is
///   `None` on Darwin.
/// - Windows: `data_local_dir()` (i.e. `%LOCALAPPDATA%\Alleycat\alleycat\data`).
pub fn state_dir() -> anyhow::Result<PathBuf> {
    let dirs = project_dirs()?;
    let path = if let Some(state) = dirs.state_dir() {
        state.to_path_buf()
    } else if cfg!(target_os = "linux") {
        // Defensive: ProjectDirs always returns Some(state) on Linux, but if
        // for some reason it didn't, fall back through BaseDirs.
        let base = base_dirs()?;
        let state = base
            .state_dir()
            .ok_or_else(|| anyhow!("no XDG state dir on this Linux system"))?;
        state.join(APPLICATION)
    } else if cfg!(target_os = "windows") {
        dirs.data_local_dir().to_path_buf()
    } else {
        // macOS and any other unix: collapse onto config_dir.
        dirs.config_dir().to_path_buf()
    };
    ensure_dir(&path)?;
    Ok(path)
}

/// Per-user log directory.
pub fn log_dir() -> anyhow::Result<PathBuf> {
    let path: PathBuf;
    #[cfg(target_os = "macos")]
    {
        let base = base_dirs()?;
        path = base.home_dir().join("Library/Logs").join(PROJECT_ID);
    }
    #[cfg(target_os = "linux")]
    {
        path = state_dir()?.join("logs");
    }
    #[cfg(target_os = "windows")]
    {
        let dirs = project_dirs()?;
        path = dirs.data_local_dir().join("logs");
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        path = state_dir()?.join("logs");
    }
    ensure_dir(&path)?;
    Ok(path)
}

/// Path to the TOML config file (`<config_dir>/config.toml`).
pub fn config_file() -> anyhow::Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

pub fn cert_file() -> anyhow::Result<PathBuf> {
    Ok(state_dir()?.join("cert.pem"))
}

pub fn key_file() -> anyhow::Result<PathBuf> {
    Ok(state_dir()?.join("key.pem"))
}

pub fn token_file() -> anyhow::Result<PathBuf> {
    Ok(state_dir()?.join("token"))
}

pub fn port_file() -> anyhow::Result<PathBuf> {
    Ok(state_dir()?.join("port"))
}

pub fn lock_file() -> anyhow::Result<PathBuf> {
    Ok(state_dir()?.join("lock"))
}

/// `sockaddr_un.sun_path` size on Darwin/BSD (104) and Linux (108). The
/// kernel needs a trailing NUL inside that buffer, so the longest usable
/// path is one byte shorter.
#[cfg(target_os = "linux")]
const SUN_PATH_MAX: usize = 108 - 1;
#[cfg(not(target_os = "linux"))]
const SUN_PATH_MAX: usize = 104 - 1;

/// Unix domain socket path the daemon listens on. Errors on Windows — call
/// [`control_pipe_name`] there instead.
///
/// macOS limits `sockaddr_un.sun_path` to 104 bytes (Linux: 108). The default
/// state dir (`~/Library/Application Support/dev.Alleycat.alleycat/`) plus
/// `control.sock` already eats ~70 chars on macOS, and hermetic test homes
/// rooted at `/var/folders/.../T/` blow the limit outright. To keep the
/// socket bindable everywhere we put it under a short tmpdir on macOS/BSD
/// (and on Linux when `$XDG_RUNTIME_DIR` is available); the rest of state
/// (`cert.pem`, `key.pem`, `token`, `port`, `lock`) stays under `state_dir()`
/// — those files aren't subject to the sockaddr_un cap.
pub fn control_socket_path() -> anyhow::Result<PathBuf> {
    #[cfg(unix)]
    {
        let base = base_dirs()?;
        let home = base.home_dir().to_string_lossy().to_string();
        let user = short_user_hash(&home);
        let dir = control_socket_dir(&user)?;
        ensure_dir(&dir)?;
        let sock = dir.join("control.sock");
        let len = sock.as_os_str().len();
        if len > SUN_PATH_MAX {
            return Err(anyhow!(
                "control socket path is {len} bytes, exceeds sockaddr_un limit of {SUN_PATH_MAX}: {}",
                sock.display()
            ));
        }
        Ok(sock)
    }
    #[cfg(not(unix))]
    {
        Err(anyhow!(
            "control_socket_path() is unix-only; use control_pipe_name() on this platform"
        ))
    }
}

/// Pick the parent directory for the control socket. Tries, in order:
///
/// 1. `$XDG_RUNTIME_DIR/alleycat-<userhash>` (Linux ideal — tmpfs, per-user)
/// 2. `<state_dir>/alleycat-<userhash>` (Linux fallback — XDG_STATE_HOME is
///    short enough that this normally fits)
/// 3. `$TMPDIR/alleycat-<userhash>` (macOS / BSD — `$TMPDIR` is per-user on
///    macOS and short enough for the limit)
/// 4. `/tmp/alleycat-<userhash>` (last-resort fallback for everywhere)
///
/// The first candidate whose resulting `<dir>/control.sock` fits in
/// `SUN_PATH_MAX` wins. We always include `<userhash>` (first 8 hex of
/// SHA-256 of `$HOME`) in the directory name so multiple accounts sharing
/// `/tmp` don't collide and so daemon + CLI agree on the same path.
#[cfg(unix)]
fn control_socket_dir(user: &str) -> anyhow::Result<PathBuf> {
    let segment = format!("alleycat-{user}");
    let mut candidates: Vec<PathBuf> = Vec::new();

    #[cfg(target_os = "linux")]
    {
        if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
            let rt = PathBuf::from(rt);
            if rt.is_absolute() {
                candidates.push(rt.join(&segment));
            }
        }
        if let Ok(state) = state_dir() {
            candidates.push(state.join(&segment));
        }
    }

    if let Some(tmp) = std::env::var_os("TMPDIR") {
        let tmp = PathBuf::from(tmp);
        if tmp.is_absolute() {
            candidates.push(tmp.join(&segment));
        }
    }
    candidates.push(PathBuf::from("/tmp").join(&segment));

    let sock_overhead = "/control.sock".len();
    for cand in &candidates {
        let total = cand.as_os_str().len() + sock_overhead;
        if total <= SUN_PATH_MAX {
            return Ok(cand.clone());
        }
    }
    Err(anyhow!(
        "no candidate directory yields a control-socket path within the {SUN_PATH_MAX}-byte sockaddr_un limit; tried: {candidates:?}"
    ))
}

/// Windows named-pipe name for the control IPC. Errors on non-Windows
/// platforms — call [`control_socket_path`] there instead.
///
/// Form: `\\.\pipe\alleycat-control-<userhash>`. The hash is derived from
/// the user's home directory path so two accounts on the same machine get
/// distinct pipe names without pulling in `windows-sys` to query the SID.
pub fn control_pipe_name() -> anyhow::Result<String> {
    #[cfg(windows)]
    {
        let base = base_dirs()?;
        let home = base.home_dir().to_string_lossy().to_string();
        let hash = short_user_hash(&home);
        Ok(format!(r"\\.\pipe\alleycat-control-{hash}"))
    }
    #[cfg(not(windows))]
    {
        Err(anyhow!(
            "control_pipe_name() is windows-only; use control_socket_path() on this platform"
        ))
    }
}

fn short_user_hash(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(input.as_bytes());
    hex::encode(&digest[..8])
}

/// Path to the launchd user-agent plist on macOS.
/// `~/Library/LaunchAgents/dev.alleycat.alleycat.plist`.
pub fn launchd_plist_path() -> anyhow::Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let base = base_dirs()?;
        Ok(base
            .home_dir()
            .join("Library/LaunchAgents")
            .join(format!("{LAUNCHD_LABEL}.plist")))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(anyhow!("launchd_plist_path() is macOS-only"))
    }
}

/// Path to the systemd user unit file on Linux.
/// `$XDG_CONFIG_HOME/systemd/user/alleycat.service` (defaults to
/// `~/.config/systemd/user/alleycat.service`).
pub fn systemd_unit_path() -> anyhow::Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let base = base_dirs()?;
        Ok(base
            .config_dir()
            .join("systemd")
            .join("user")
            .join(format!("{APPLICATION}.service")))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(anyhow!("systemd_unit_path() is Linux-only"))
    }
}

/// Path to the Windows Startup-folder shortcut.
/// `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\alleycat.lnk`.
pub fn windows_startup_lnk_path() -> anyhow::Result<PathBuf> {
    #[cfg(windows)]
    {
        let base = base_dirs()?;
        // BaseDirs::config_dir() on Windows == %APPDATA% (Roaming).
        Ok(base
            .config_dir()
            .join(r"Microsoft\Windows\Start Menu\Programs\Startup")
            .join("alleycat.lnk"))
    }
    #[cfg(not(windows))]
    {
        Err(anyhow!("windows_startup_lnk_path() is Windows-only"))
    }
}

/// XDG autostart fallback for Linux desktops without `systemctl --user`.
/// `~/.config/autostart/alleycat.desktop`.
#[cfg(target_os = "linux")]
pub fn xdg_autostart_path() -> anyhow::Result<PathBuf> {
    let base = base_dirs()?;
    Ok(base
        .config_dir()
        .join("autostart")
        .join(format!("{APPLICATION}.desktop")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempHome;

    #[test]
    fn config_dir_under_temp_home() {
        let h = TempHome::new();
        let cfg = config_dir().expect("config_dir");
        assert!(
            cfg.starts_with(h.path()),
            "{} should be under {}",
            cfg.display(),
            h.path().display()
        );
        assert!(cfg.is_dir(), "config_dir must be created");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&cfg).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "config_dir should be mode 0700");
        }
    }

    #[test]
    fn config_file_lives_under_config_dir() {
        let _h = TempHome::new();
        let cf = config_file().expect("config_file");
        assert_eq!(cf.file_name().unwrap(), "config.toml");
        assert!(cf.starts_with(config_dir().unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn control_socket_fits_sockaddr_un_limit() {
        use std::os::unix::fs::PermissionsExt;
        let _h = TempHome::new();
        let sock = control_socket_path().expect("control_socket_path");
        let len = sock.as_os_str().len();
        assert!(
            len <= SUN_PATH_MAX,
            "control socket path is {len} bytes, exceeds {SUN_PATH_MAX}: {}",
            sock.display()
        );
        assert_eq!(sock.file_name().unwrap(), "control.sock");
        // Parent dir must exist with mode 0700.
        let parent = sock.parent().unwrap();
        assert!(parent.is_dir(), "{} should be created", parent.display());
        let mode = std::fs::metadata(parent).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{} should be 0700", parent.display());
    }

    #[cfg(unix)]
    #[test]
    fn control_socket_includes_userhash_segment() {
        // Two different HOMEs should yield different socket directories so
        // multi-user `/tmp` sharing doesn't collide.
        let h1 = TempHome::new();
        let s1 = control_socket_path().expect("first");
        drop(h1);

        let h2 = TempHome::new();
        let s2 = control_socket_path().expect("second");
        drop(h2);

        assert_ne!(
            s1.parent().unwrap(),
            s2.parent().unwrap(),
            "different HOMEs should pick different socket dirs"
        );
        for s in [&s1, &s2] {
            let dir = s.parent().unwrap().file_name().unwrap().to_string_lossy();
            assert!(
                dir.starts_with("alleycat-"),
                "expected alleycat-<userhash>/ segment, got {dir}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn control_socket_under_pathological_long_home() {
        // Hermetic test homes under `/var/folders/.../T/...` plus the macOS
        // state_dir suffix routinely exceed 104 bytes. Confirm we still
        // produce a bindable path.
        let mut h = TempHome::new();
        // Force TMPDIR to be very long — emulates a deep mktemp -d sandbox.
        let long_tmp = h
            .path()
            .join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/bbbbbbbbbbbbbbbbbbbbbbbb/cccc");
        std::fs::create_dir_all(&long_tmp).unwrap();
        h.override_env(&[("TMPDIR", long_tmp.to_str().unwrap())]);

        let sock = control_socket_path().expect("path within sun limit");
        assert!(
            sock.as_os_str().len() <= SUN_PATH_MAX,
            "len={} path={}",
            sock.as_os_str().len(),
            sock.display()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn control_socket_prefers_xdg_runtime_dir_when_set() {
        let mut h = TempHome::new();
        let runtime = h.path().join("xdg-runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        h.override_env(&[("XDG_RUNTIME_DIR", runtime.to_str().unwrap())]);
        let sock = control_socket_path().expect("control_socket_path");
        assert!(
            sock.starts_with(&runtime),
            "{} should be under {}",
            sock.display(),
            runtime.display()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn launchd_plist_path_label() {
        let _h = TempHome::new();
        let p = launchd_plist_path().expect("launchd_plist_path");
        assert_eq!(p.file_name().unwrap(), "dev.alleycat.alleycat.plist");
        assert!(p.to_string_lossy().contains("Library/LaunchAgents"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn systemd_unit_path_endings() {
        let _h = TempHome::new();
        let p = systemd_unit_path().expect("systemd_unit_path");
        let s = p.to_string_lossy();
        assert!(s.ends_with("systemd/user/alleycat.service"), "{s}");
    }

    #[cfg(windows)]
    #[test]
    fn windows_startup_lnk_path_endings() {
        let _h = TempHome::new();
        let p = windows_startup_lnk_path().expect("windows_startup_lnk_path");
        let s = p.to_string_lossy().to_string();
        assert!(
            s.ends_with(r"Startup\alleycat.lnk"),
            "expected to end with Startup\\alleycat.lnk, got {s}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_state_collapses_to_config() {
        let _h = TempHome::new();
        assert_eq!(state_dir().unwrap(), config_dir().unwrap());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_log_dir_under_library_logs() {
        let _h = TempHome::new();
        let p = log_dir().unwrap();
        let s = p.to_string_lossy();
        assert!(s.contains("Library/Logs/dev.Alleycat.alleycat"), "{s}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_state_under_xdg_state_home() {
        let mut h = TempHome::new();
        let state_home = h.path().join("xdg-state");
        std::fs::create_dir_all(&state_home).unwrap();
        h.override_env(&[("XDG_STATE_HOME", state_home.to_str().unwrap())]);
        let s = state_dir().unwrap();
        assert!(
            s.starts_with(&state_home),
            "expected {} under {}",
            s.display(),
            state_home.display()
        );
        let logs = log_dir().unwrap();
        assert!(logs.starts_with(&s), "logs under state");
        assert_eq!(logs.file_name().unwrap(), "logs");
    }
}
