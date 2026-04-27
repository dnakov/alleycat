//! TOML configuration for the alleycat daemon. Persisted at
//! [`crate::paths::config_file`].
//!
//! Schema:
//! ```toml
//! [relay]
//! bind = "0.0.0.0"
//! udp_port = 0          # 0 = let OS pick on first run; persisted port wins later
//!
//! [allowlist]
//! tcp = ["127.0.0.1:8390"]
//! unix = []
//!
//! [host]
//! overrides = []         # empty = host_detect::detect_candidates()
//!
//! [log]
//! level = "info"         # passed to tracing EnvFilter
//! ```
//!
//! `load_or_init()` writes the default file if none exists. `save()` is
//! atomic (write-tmp, fsync, rename) and chmod-0600 on Unix.
//! `diff_requires_restart()` flags edits to bind / udp_port — Quinn's
//! listening socket can't move underneath live sessions.

use std::path::Path;

use alleycat_protocol::Target;
use anyhow::{Context, anyhow};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::paths;

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    pub relay: RelaySection,
    pub allowlist: AllowlistSection,
    pub host: HostSection,
    pub log: LogSection,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct RelaySection {
    pub bind: String,
    pub udp_port: u16,
}

impl Default for RelaySection {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0".to_string(),
            udp_port: 0,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct AllowlistSection {
    pub tcp: Vec<String>,
    pub unix: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct HostSection {
    pub overrides: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct LogSection {
    pub level: String,
}

impl Default for LogSection {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
        }
    }
}

/// Load config from disk, or write+return the default if no file exists.
pub async fn load_or_init() -> anyhow::Result<Config> {
    let path = paths::config_file()?;
    if !path.exists() {
        let cfg = Config::default();
        save(&cfg).await?;
        return Ok(cfg);
    }
    let raw = fs::read_to_string(&path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    let cfg: Config =
        toml::from_str(&raw).with_context(|| format!("parsing TOML in {}", path.display()))?;
    Ok(cfg)
}

/// Write config to disk atomically. mode 0600 on Unix.
pub async fn save(cfg: &Config) -> anyhow::Result<()> {
    let path = paths::config_file()?;
    let body = toml::to_string_pretty(cfg).context("serializing config to TOML")?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("config path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .await
        .with_context(|| format!("creating {}", parent.display()))?;
    let tmp = tmp_path(&path);
    {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .await
            .with_context(|| format!("opening {}", tmp.display()))?;
        f.write_all(body.as_bytes())
            .await
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.flush().await.ok();
        f.sync_all()
            .await
            .with_context(|| format!("fsync {}", tmp.display()))?;
    }
    set_mode_0600(&tmp).ok();
    fs::rename(&tmp, &path)
        .await
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Convert the TOML allowlist into the protocol's `Target` enum.
/// Invalid TCP entries are skipped silently — the daemon will log them
/// during startup; we don't want a stray bad string to crash the process.
pub fn parse_targets(cfg: &Config) -> Vec<Target> {
    let mut out = Vec::with_capacity(cfg.allowlist.tcp.len() + cfg.allowlist.unix.len());
    for s in &cfg.allowlist.tcp {
        if let Ok(t) = parse_tcp_target(s) {
            out.push(t);
        }
    }
    for path in &cfg.allowlist.unix {
        out.push(Target::Unix { path: path.clone() });
    }
    out
}

/// Parse a `host:port` string into a `Target::Tcp`. Lifted out of the
/// previous `main.rs::parse_allow_tcp` helper so both clap and the daemon
/// share one parser.
pub fn parse_tcp_target(value: &str) -> Result<Target, String> {
    let (host, port) = value
        .rsplit_once(':')
        .ok_or_else(|| "expected host:port".to_string())?;
    if host.is_empty() {
        return Err("expected host:port (host was empty)".to_string());
    }
    let port = port
        .parse::<u16>()
        .map_err(|error| format!("invalid port: {error}"))?;
    Ok(Target::Tcp {
        host: host.to_string(),
        port,
    })
}

/// Returns Some(reason) if applying `new` over `old` would require a daemon
/// restart. Bind addr / UDP port changes can't be hot-reloaded — Quinn's
/// listening socket is bound at startup.
pub fn diff_requires_restart(old: &Config, new: &Config) -> Option<&'static str> {
    if old.relay.bind != new.relay.bind {
        return Some("relay.bind changed; restart required");
    }
    if old.relay.udp_port != new.relay.udp_port {
        return Some("relay.udp_port changed; restart required");
    }
    None
}

fn tmp_path(target: &Path) -> std::path::PathBuf {
    let mut name = target
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    target.with_file_name(name)
}

fn set_mode_0600(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 0600 {}", path.display()))?;
    }
    #[cfg(windows)]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempHome;

    #[tokio::test]
    async fn missing_file_yields_default() {
        let _h = TempHome::new();
        let cfg = load_or_init().await.expect("load_or_init");
        assert_eq!(cfg, Config::default());
        // The file should now exist on disk.
        assert!(paths::config_file().unwrap().exists());
    }

    #[tokio::test]
    async fn round_trip_via_disk() {
        let _h = TempHome::new();
        let mut cfg = Config::default();
        cfg.allowlist.tcp.push("127.0.0.1:8390".to_string());
        cfg.allowlist.unix.push("/tmp/foo.sock".to_string());
        cfg.host.overrides.push("studio.tailnet.ts.net".to_string());
        cfg.log.level = "debug".to_string();
        save(&cfg).await.expect("save");
        let got = load_or_init().await.expect("reload");
        assert_eq!(got, cfg);
    }

    #[test]
    fn parse_tcp_target_accepts_host_port() {
        let t = parse_tcp_target("127.0.0.1:8390").expect("ok");
        assert_eq!(
            t,
            Target::Tcp {
                host: "127.0.0.1".into(),
                port: 8390
            }
        );
    }

    #[test]
    fn parse_tcp_target_rejects_garbage() {
        assert!(parse_tcp_target("nope").is_err());
        assert!(parse_tcp_target(":8390").is_err());
        assert!(parse_tcp_target("host:abc").is_err());
    }

    #[test]
    fn parse_targets_handles_tcp_and_unix() {
        let cfg = Config {
            allowlist: AllowlistSection {
                tcp: vec!["127.0.0.1:8390".into(), "bad-line".into()],
                unix: vec!["/var/run/litter.sock".into()],
            },
            ..Default::default()
        };
        let targets = parse_targets(&cfg);
        assert_eq!(targets.len(), 2, "bad-line should be skipped");
        assert!(matches!(targets[0], Target::Tcp { .. }));
        assert!(matches!(targets[1], Target::Unix { .. }));
    }

    #[test]
    fn diff_requires_restart_flags_bind_change() {
        let mut a = Config::default();
        let mut b = Config::default();
        assert!(diff_requires_restart(&a, &b).is_none());
        b.relay.bind = "127.0.0.1".to_string();
        assert!(diff_requires_restart(&a, &b).is_some());
        a.relay.bind = "127.0.0.1".to_string();
        assert!(diff_requires_restart(&a, &b).is_none());
    }

    #[test]
    fn diff_requires_restart_flags_port_change() {
        let mut a = Config::default();
        let mut b = Config::default();
        b.relay.udp_port = 41234;
        assert!(diff_requires_restart(&a, &b).is_some());
        a.relay.udp_port = 41234;
        assert!(diff_requires_restart(&a, &b).is_none());
    }

    #[test]
    fn diff_requires_restart_ignores_allowlist_change() {
        let mut a = Config::default();
        let mut b = Config::default();
        a.allowlist.tcp.push("127.0.0.1:1".into());
        b.allowlist.tcp.push("127.0.0.1:2".into());
        b.host.overrides.push("alt.local".into());
        b.log.level = "trace".into();
        assert!(diff_requires_restart(&a, &b).is_none());
    }

    #[tokio::test]
    async fn partial_toml_uses_defaults() {
        let _h = TempHome::new();
        let path = paths::config_file().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Only specify [allowlist].
        std::fs::write(
            &path,
            "[allowlist]\ntcp = [\"127.0.0.1:9000\"]\nunix = []\n",
        )
        .unwrap();
        let cfg = load_or_init().await.expect("load");
        assert_eq!(cfg.relay, RelaySection::default());
        assert_eq!(cfg.host, HostSection::default());
        assert_eq!(cfg.log, LogSection::default());
        assert_eq!(cfg.allowlist.tcp, vec!["127.0.0.1:9000"]);
    }
}
