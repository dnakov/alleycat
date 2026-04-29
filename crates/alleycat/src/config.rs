use std::path::{Path, PathBuf};

use anyhow::Context;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::paths;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct HostConfig {
    pub token: String,
    pub relay: Option<String>,
    pub agents: AgentsConfig,
    pub session: SessionConfig,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            token: random_token(),
            relay: None,
            agents: AgentsConfig::default(),
            session: SessionConfig::default(),
        }
    }
}

/// Knobs for the daemon-lifetime session registry. Defaults are tuned for a
/// phone client that may sit on a flaky relay path: 16 MiB of replay history
/// covers a long turn, the 10-minute idle TTL keeps a session warm across
/// short backgrounding, and the 60-second pending grace lets a reconnecting
/// client re-render approval prompts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SessionConfig {
    pub replay_max_msgs: usize,
    pub replay_max_bytes: usize,
    pub idle_ttl_secs: u64,
    pub pending_grace_secs: u64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            replay_max_msgs: 2048,
            replay_max_bytes: 16 << 20,
            idle_ttl_secs: 600,
            pending_grace_secs: 60,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AgentsConfig {
    pub codex: CodexAgentConfig,
    pub pi: PiAgentConfig,
    pub opencode: OpencodeAgentConfig,
    pub claude: ClaudeAgentConfig,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            codex: CodexAgentConfig::default(),
            pi: PiAgentConfig::default(),
            opencode: OpencodeAgentConfig::default(),
            claude: ClaudeAgentConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CodexAgentConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
}

impl Default for CodexAgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            host: "127.0.0.1".to_string(),
            port: 8390,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PiAgentConfig {
    pub enabled: bool,
    pub bin: String,
}

impl Default for PiAgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bin: "pi-coding-agent".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct OpencodeAgentConfig {
    pub enabled: bool,
    pub bin: String,
}

impl Default for OpencodeAgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bin: "opencode".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ClaudeAgentConfig {
    pub enabled: bool,
    pub bin: String,
    /// When true (default), spawn `claude` with `--dangerously-skip-permissions`
    /// (matches the user's local `claude` shell alias). When false, spawn with
    /// `--permission-prompt-tool stdio` and bridge each `can_use_tool`
    /// inbound control_request to a codex `requestApproval` server→client
    /// request — the connected phone client gets to approve/deny each tool
    /// call.
    pub bypass_permissions: bool,
}

impl Default for ClaudeAgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bin: "claude".to_string(),
            bypass_permissions: true,
        }
    }
}

pub async fn load_or_init() -> anyhow::Result<HostConfig> {
    let path = paths::host_config_file()?;
    match fs::read_to_string(&path).await {
        Ok(raw) => toml::from_str(&raw).with_context(|| format!("parsing {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let cfg = HostConfig::default();
            save(&cfg).await?;
            Ok(cfg)
        }
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

pub async fn save(config: &HostConfig) -> anyhow::Result<()> {
    let path = paths::host_config_file()?;
    let data = toml::to_string_pretty(config).context("serializing host config")?;
    atomic_write(&path, data.as_bytes()).await?;
    Ok(())
}

pub async fn rotate_token() -> anyhow::Result<HostConfig> {
    let mut config = load_or_init().await?;
    config.token = random_token();
    save(&config).await?;
    Ok(config)
}

pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

async fn atomic_write(target: &Path, contents: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = tmp_path(target);
    {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .await
            .with_context(|| format!("opening {}", tmp.display()))?;
        file.write_all(contents)
            .await
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.flush().await.ok();
        file.sync_all().await.ok();
    }
    set_mode_0600(&tmp)?;
    fs::rename(&tmp, target)
        .await
        .with_context(|| format!("renaming {} -> {}", tmp.display(), target.display()))?;
    set_mode_0600(target)?;
    Ok(())
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    PathBuf::from(tmp)
}

#[cfg(unix)]
fn set_mode_0600(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 0600 {}", path.display()))
}

#[cfg(not(unix))]
fn set_mode_0600(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_token_is_32_bytes_hex() {
        let token = random_token();
        assert_eq!(token.len(), 64);
        let decoded = hex::decode(token).unwrap();
        assert_eq!(decoded.len(), 32);
    }

    #[test]
    fn default_config_routes_codex_to_local_app_server() {
        let config = HostConfig::default();
        assert!(config.agents.codex.enabled);
        assert_eq!(config.agents.codex.host, "127.0.0.1");
        assert_eq!(config.agents.codex.port, 8390);
        assert!(config.agents.pi.enabled);
        assert!(config.agents.opencode.enabled);
        assert!(config.agents.claude.enabled);
    }
}
