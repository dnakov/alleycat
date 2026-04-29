use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use alleycat_bridge_core::session::{Session, SessionRegistry, SessionRegistryConfig};
use alleycat_bridge_core::{Bridge, Conn, JsonRpcError, LocalLauncher};
use alleycat_claude_bridge::ClaudeBridge;
use alleycat_opencode_bridge::OpencodeBridge;
use alleycat_pi_bridge::PiBridge;
use anyhow::{Context, anyhow};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::OnceCell;

use crate::config::HostConfig;
use crate::protocol::{AgentInfo, AgentWire};
use crate::stream::IrohStream;

/// Stable identifier for a JSON-RPC bridge agent. Codex is intentionally
/// excluded — it's a transparent TCP byte-pump, not a `Bridge` impl.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AgentKind {
    Pi,
    Claude,
    Opencode,
}

#[derive(Clone)]
pub struct AgentManager {
    config: Arc<ArcSwap<HostConfig>>,
    bridges: HashMap<AgentKind, Arc<dyn Bridge>>,
    /// Opencode is built lazily because constructing it spawns the opencode
    /// child + opens an SSE subscription; we don't want to pay that cost on
    /// daemon startup if no client ever asks for opencode.
    opencode_bridge: Arc<OnceCell<Arc<OpencodeBridge>>>,
    session_registry: Arc<SessionRegistry>,
    /// Held to keep the registry's reaper alive for the daemon lifetime.
    _reaper_handle: Arc<tokio::task::JoinHandle<()>>,
}

impl AgentManager {
    pub async fn new(config: Arc<ArcSwap<HostConfig>>) -> anyhow::Result<Self> {
        let snapshot = config.load();

        // Honor `CODEX_HOME` so the user can point the bridge thread indices
        // at the same on-disk session store their `pi-coding-agent` / `codex`
        // CLI already uses (typically `~/.codex`). Each bridge falls back to
        // its own OS-conventional default when unset.
        let codex_home = match std::env::var_os("CODEX_HOME") {
            Some(value) if !value.is_empty() => Some(PathBuf::from(value)),
            _ => None,
        };
        if let Some(ref home) = codex_home {
            tokio::fs::create_dir_all(home)
                .await
                .with_context(|| format!("creating {}", home.display()))?;
        }

        let mut pi_builder = PiBridge::builder()
            .agent_bin(snapshot.agents.pi.bin.clone())
            .launcher(Arc::new(LocalLauncher));
        if let Some(ref home) = codex_home {
            pi_builder = pi_builder.codex_home(home.clone());
        }
        let pi_bridge = pi_builder.build().await.context("building pi bridge")?;

        let mut claude_builder = ClaudeBridge::builder()
            .agent_bin(snapshot.agents.claude.bin.clone())
            .launcher(Arc::new(LocalLauncher))
            .bypass_permissions(snapshot.agents.claude.bypass_permissions);
        if let Some(ref home) = codex_home {
            claude_builder = claude_builder.codex_home(home.clone());
        }
        let claude_bridge = claude_builder
            .build()
            .await
            .context("building claude bridge")?;

        let mut bridges: HashMap<AgentKind, Arc<dyn Bridge>> = HashMap::new();
        bridges.insert(AgentKind::Pi, pi_bridge as Arc<dyn Bridge>);
        bridges.insert(AgentKind::Claude, claude_bridge as Arc<dyn Bridge>);

        let session_cfg = &snapshot.session;
        let registry_config = SessionRegistryConfig {
            ring_max_msgs: session_cfg.replay_max_msgs,
            ring_max_bytes: session_cfg.replay_max_bytes,
            idle_ttl: std::time::Duration::from_secs(session_cfg.idle_ttl_secs),
            pending_grace: std::time::Duration::from_secs(session_cfg.pending_grace_secs),
        };
        let session_registry = SessionRegistry::new(registry_config);
        let reaper_handle = Arc::new(session_registry.spawn_reaper());

        Ok(Self {
            config,
            bridges,
            opencode_bridge: Arc::new(OnceCell::new()),
            session_registry,
            _reaper_handle: reaper_handle,
        })
    }

    pub fn session_registry(&self) -> &Arc<SessionRegistry> {
        &self.session_registry
    }

    pub async fn list_agents(&self) -> Vec<AgentInfo> {
        vec![
            AgentInfo {
                name: "codex".to_string(),
                display_name: "Codex".to_string(),
                wire: AgentWire::Websocket,
                available: self.codex_available().await,
            },
            AgentInfo {
                name: "pi".to_string(),
                display_name: "Pi".to_string(),
                wire: AgentWire::Jsonl,
                available: self.pi_available(),
            },
            AgentInfo {
                name: "opencode".to_string(),
                display_name: "OpenCode".to_string(),
                wire: AgentWire::Jsonl,
                available: self.opencode_available(),
            },
            AgentInfo {
                name: "claude".to_string(),
                display_name: "Claude".to_string(),
                wire: AgentWire::Jsonl,
                available: self.claude_available(),
            },
        ]
    }

    /// Session-aware dispatch: the iroh stream attaches to the supplied
    /// session and survives a client disconnect.
    pub async fn serve_agent_with_session(
        &self,
        agent: &str,
        stream: IrohStream,
        session: Arc<Session>,
        last_seen: Option<u64>,
    ) -> anyhow::Result<()> {
        match agent {
            // Codex doesn't participate in the JSON-RPC replay scheme — it's
            // a transparent TCP byte-pump to the codex app-server, which has
            // its own resume semantics (SQLite session store). The session
            // is held just so the registry's accounting stays uniform; its
            // ring stays empty.
            "codex" => {
                let _ = (session, last_seen);
                self.serve_codex(stream).await
            }
            other => {
                let kind =
                    agent_kind_from_str(other).ok_or_else(|| anyhow!("unknown agent `{other}`"))?;
                self.serve_with_session(kind, stream, session, last_seen)
                    .await
            }
        }
    }

    /// Polymorphic Bridge dispatch. Pi/Claude come straight from the eagerly-
    /// built `bridges` map; opencode initializes lazily on first use.
    pub async fn serve_with_session<S>(
        &self,
        kind: AgentKind,
        stream: S,
        session: Arc<Session>,
        last_seen: Option<u64>,
    ) -> anyhow::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        if !self.config.load().agents.is_enabled(kind) {
            return Err(anyhow!("agent `{}` is disabled", agent_kind_str(kind)));
        }
        let bridge: Arc<dyn Bridge> =
            match kind {
                AgentKind::Opencode => {
                    let oc = self.opencode_bridge_arc().await?;
                    oc as Arc<dyn Bridge>
                }
                other => self.bridges.get(&other).cloned().ok_or_else(|| {
                    anyhow!("agent `{}` is not configured", agent_kind_str(other))
                })?,
            };
        alleycat_bridge_core::serve_stream_with_session(bridge, stream, session, last_seen)
            .await
            .with_context(|| format!("serving `{}` bridge stream", agent_kind_str(kind)))
    }

    /// Stable static name for a wire-supplied agent string, used to key the
    /// session registry. Returns `None` for unknown agents.
    pub fn agent_id(name: &str) -> Option<&'static str> {
        match name {
            "codex" => Some("codex"),
            "pi" => Some("pi"),
            "opencode" => Some("opencode"),
            "claude" => Some("claude"),
            _ => None,
        }
    }

    pub fn agent_enabled(&self, agent: &str) -> bool {
        let cfg = self.config.load();
        match agent {
            "codex" => cfg.agents.codex.enabled,
            "pi" => cfg.agents.pi.enabled,
            "opencode" => cfg.agents.opencode.enabled,
            "claude" => cfg.agents.claude.enabled,
            _ => false,
        }
    }

    async fn serve_codex(&self, mut iroh_stream: IrohStream) -> anyhow::Result<()> {
        let cfg = self.config.load();
        let codex = &cfg.agents.codex;
        if !codex.enabled {
            return Err(anyhow!("codex agent is disabled"));
        }
        let host = codex.host.clone();
        let port = codex.port;
        drop(cfg);
        let mut tcp = TcpStream::connect((host.as_str(), port))
            .await
            .with_context(|| format!("connecting to Codex at {host}:{port}"))?;
        let _ = tokio::io::copy_bidirectional(&mut iroh_stream, &mut tcp).await;
        Ok(())
    }

    async fn opencode_bridge_arc(&self) -> anyhow::Result<Arc<OpencodeBridge>> {
        let bin = {
            let cfg = self.config.load();
            if !cfg.agents.opencode.enabled {
                return Err(anyhow!("opencode agent is disabled"));
            }
            cfg.agents.opencode.bin.clone()
        };
        let bridge = self
            .opencode_bridge
            .get_or_try_init(|| async {
                // `OpencodeBridgeBuilder::from_env()` reads
                // `OPENCODE_BRIDGE_BIN` (and friends) at `build()` time; the
                // host config's `opencode.bin` overrides whatever the parent
                // shell set. Mirror the pre-A5 daemon behavior.
                unsafe {
                    std::env::set_var("OPENCODE_BRIDGE_BIN", &bin);
                }
                OpencodeBridge::builder()
                    .from_env()
                    .build()
                    .await
                    .context("initializing opencode bridge")
            })
            .await?;
        Ok(Arc::clone(bridge))
    }

    async fn codex_available(&self) -> bool {
        let cfg = self.config.load();
        let codex = &cfg.agents.codex;
        if !codex.enabled {
            return false;
        }
        let addr = (codex.host.clone(), codex.port);
        drop(cfg);
        tokio::time::timeout(Duration::from_millis(250), TcpStream::connect(addr))
            .await
            .is_ok_and(|result| result.is_ok())
    }

    fn pi_available(&self) -> bool {
        let cfg = self.config.load();
        cfg.agents.pi.enabled && which::which(&cfg.agents.pi.bin).is_ok()
    }

    fn opencode_available(&self) -> bool {
        let cfg = self.config.load();
        cfg.agents.opencode.enabled
            && (std::env::var_os("OPENCODE_BRIDGE_BACKEND_URL").is_some()
                || which::which(&cfg.agents.opencode.bin).is_ok())
    }

    fn claude_available(&self) -> bool {
        let cfg = self.config.load();
        cfg.agents.claude.enabled && which::which(&cfg.agents.claude.bin).is_ok()
    }
}

fn agent_kind_from_str(name: &str) -> Option<AgentKind> {
    match name {
        "pi" => Some(AgentKind::Pi),
        "claude" => Some(AgentKind::Claude),
        "opencode" => Some(AgentKind::Opencode),
        _ => None,
    }
}

fn agent_kind_str(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Pi => "pi",
        AgentKind::Claude => "claude",
        AgentKind::Opencode => "opencode",
    }
}

impl crate::config::AgentsConfig {
    fn is_enabled(&self, kind: AgentKind) -> bool {
        match kind {
            AgentKind::Pi => self.pi.enabled,
            AgentKind::Claude => self.claude.enabled,
            AgentKind::Opencode => self.opencode.enabled,
        }
    }
}
