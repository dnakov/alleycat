use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alleycat_bridge_core::session::{Session, SessionRegistry, SessionRegistryConfig};
use alleycat_bridge_core::{Bridge, Conn, JsonRpcError, LocalLauncher};
use alleycat_claude_bridge::ClaudeBridge;
use alleycat_opencode_bridge::OpencodeBridge;
use alleycat_pi_bridge::PiBridge;
use anyhow::{Context, anyhow};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, OnceCell};
use tracing::warn;

use crate::config::HostConfig;
use crate::protocol::{AgentInfo, AgentWire};
use crate::stream::IrohStream;

/// Stable identifier for a JSON-RPC bridge agent. Codex is intentionally
/// excluded — the daemon runs a single shared `codex app-server` in
/// websocket-listen mode and byte-pumps each iroh stream straight to it.
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
    /// One shared `codex app-server --listen ws://...` for the daemon
    /// lifetime, lazy-spawned on first `connect`. Codex multiplexes
    /// conversations internally, so each iroh stream is its own websocket
    /// client and the child stays up across client disconnects.
    codex_child: Arc<Mutex<Option<Child>>>,
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
            codex_child: Arc::new(Mutex::new(None)),
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
                available: self.codex_available(),
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
            // Codex doesn't participate in the JSON-RPC replay scheme —
            // each iroh stream is a fresh websocket client to the shared
            // codex app-server, and codex has its own resume semantics
            // (SQLite session store). The session is held just so the
            // registry's accounting stays uniform; its ring stays empty.
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
        let (host, port) = self.ensure_codex_running().await?;
        let mut tcp = TcpStream::connect((host.as_str(), port))
            .await
            .with_context(|| format!("connecting to codex app-server at {host}:{port}"))?;
        let _ = tokio::io::copy_bidirectional(&mut iroh_stream, &mut tcp).await;
        Ok(())
    }

    /// Ensures *something* is listening on the configured codex websocket
    /// address. If an externally-managed codex (or a previously-spawned
    /// child) is already accepting connections, we use it as-is and skip
    /// spawning. Otherwise we spawn `<bin> app-server --listen ws://...`
    /// and wait for the port to bind. Returns `(host, port)` for the
    /// byte-pump to dial.
    async fn ensure_codex_running(&self) -> anyhow::Result<(String, u16)> {
        let (bin, host, port) = {
            let cfg = self.config.load();
            if !cfg.agents.codex.enabled {
                return Err(anyhow!("codex agent is disabled"));
            }
            (
                cfg.agents.codex.bin.clone(),
                cfg.agents.codex.host.clone(),
                cfg.agents.codex.port,
            )
        };

        // Fast path: port is already accepting connections.
        if TcpStream::connect((host.as_str(), port)).await.is_ok() {
            return Ok((host, port));
        }

        let mut guard = self.codex_child.lock().await;

        // Re-probe under the lock so concurrent first-connects don't both
        // try to spawn.
        if TcpStream::connect((host.as_str(), port)).await.is_ok() {
            return Ok((host, port));
        }

        let child_alive = matches!(
            guard.as_mut().map(Child::try_wait),
            Some(Ok(None))
        );
        if !child_alive {
            let listen = format!("ws://{host}:{port}");
            let mut child = Command::new(&bin)
                .arg("app-server")
                .arg("--listen")
                .arg(&listen)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .with_context(|| format!("spawning `{bin} app-server --listen {listen}`"))?;

            if let Some(stderr) = child.stderr.take() {
                tokio::spawn(async move {
                    let mut lines = BufReader::new(stderr).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        warn!(target: "codex", "{line}");
                    }
                });
            }

            *guard = Some(child);
        }
        drop(guard);

        // Poll the listener until it accepts a connection. Codex usually
        // binds within a few hundred milliseconds; 5s is generous.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if TcpStream::connect((host.as_str(), port)).await.is_ok() {
                return Ok((host, port));
            }
            if Instant::now() >= deadline {
                return Err(anyhow!(
                    "codex app-server did not start listening on {host}:{port} within 5s"
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
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

    fn codex_available(&self) -> bool {
        let cfg = self.config.load();
        cfg.agents.codex.enabled && which::which(&cfg.agents.codex.bin).is_ok()
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
