use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alleycat_bridge_core::session::{Session, SessionRegistry, SessionRegistryConfig};
use alleycat_bridge_core::{Bridge, LocalLauncher};
use alleycat_claude_bridge::ClaudeBridge;
use alleycat_opencode_bridge::OpencodeBridge;
use alleycat_pi_bridge::PiBridge;
use anyhow::{Context, anyhow};
use arc_swap::ArcSwap;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, OnceCell};
use tracing::{info, warn};

use crate::config::HostConfig;
use crate::protocol::{AgentInfo, AgentWire};
use crate::stream::IrohStream;

/// Stable identifier for a JSON-RPC bridge agent. Codex is intentionally
/// excluded — the daemon talks to it directly (either over a shared
/// websocket-listen child or one stdio child per iroh stream, depending on
/// which mode the local codex binary supports).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AgentKind {
    Pi,
    Claude,
    Opencode,
}

/// How the daemon talks to `codex app-server`. Selected at startup by
/// probing `codex app-server --help` for `--listen` support, then cached
/// for the daemon's lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodexMode {
    /// `<bin> app-server --listen ws://host:port` — one shared child for
    /// the daemon lifetime, multi-client over websocket. Works on
    /// codex-cli versions that grew the `--listen` flag (≥ early 2026).
    Websocket,
    /// `<bin> app-server` — one fresh child per iroh stream, JSON-RPC
    /// over stdio. Works on every codex version that has the `app-server`
    /// subcommand.
    Stdio,
}

struct CodexDetection {
    mode: CodexMode,
    bin: PathBuf,
    available: bool,
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
    /// client and the child stays up across client disconnects. Only
    /// populated when [`AgentManager::codex_mode`] is `Websocket`.
    codex_child: Arc<Mutex<Option<Child>>>,
    /// Detected once at startup. Determines whether `serve_codex` runs the
    /// websocket byte-pump or per-stream stdio bridging.
    codex_mode: CodexMode,
    /// The resolved codex executable selected during startup probing.
    codex_bin: PathBuf,
    /// Whether the selected codex executable could be spawned.
    codex_available: bool,
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

        let codex_detection = if snapshot.agents.codex.enabled {
            detect_codex(&snapshot.agents.codex.bin).await
        } else {
            // Doesn't matter; codex is disabled. Pick a default so the
            // field has a value.
            CodexDetection {
                mode: CodexMode::Stdio,
                bin: PathBuf::from(&snapshot.agents.codex.bin),
                available: false,
            }
        };
        info!(
            codex_mode = ?codex_detection.mode,
            configured_bin = %snapshot.agents.codex.bin,
            bin = %codex_detection.bin.display(),
            available = codex_detection.available,
            "codex transport mode"
        );

        Ok(Self {
            config,
            bridges,
            opencode_bridge: Arc::new(OnceCell::new()),
            codex_child: Arc::new(Mutex::new(None)),
            codex_mode: codex_detection.mode,
            codex_bin: codex_detection.bin,
            codex_available: codex_detection.available,
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
                wire: match self.codex_mode {
                    CodexMode::Websocket => AgentWire::Websocket,
                    CodexMode::Stdio => AgentWire::Jsonl,
                },
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

    async fn serve_codex(&self, iroh_stream: IrohStream) -> anyhow::Result<()> {
        match self.codex_mode {
            CodexMode::Websocket => self.serve_codex_ws(iroh_stream).await,
            CodexMode::Stdio => self.serve_codex_stdio(iroh_stream).await,
        }
    }

    async fn serve_codex_ws(&self, mut iroh_stream: IrohStream) -> anyhow::Result<()> {
        let (host, port) = self.ensure_codex_running().await?;
        let mut tcp = TcpStream::connect((host.as_str(), port))
            .await
            .with_context(|| format!("connecting to codex app-server at {host}:{port}"))?;
        let _ = tokio::io::copy_bidirectional(&mut iroh_stream, &mut tcp).await;
        Ok(())
    }

    /// Per-stream stdio bridge for codex versions that don't support
    /// `--listen`. Each iroh stream gets its own `codex app-server` child;
    /// codex's on-disk session store handles resume across reconnects.
    async fn serve_codex_stdio(&self, mut iroh_stream: IrohStream) -> anyhow::Result<()> {
        let bin = {
            let cfg = self.config.load();
            if !cfg.agents.codex.enabled {
                return Err(anyhow!("codex agent is disabled"));
            }
            self.codex_bin.clone()
        };

        let mut child = Command::new(&bin)
            .arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning `{} app-server`", bin.display()))?;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                warn!(target: "codex", "{line}");
            }
        });

        let mut child_io = tokio::io::join(stdout, stdin);
        let _ = tokio::io::copy_bidirectional(&mut iroh_stream, &mut child_io).await;
        let _ = child.wait().await;
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
                self.codex_bin.clone(),
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

        let child_alive = matches!(guard.as_mut().map(Child::try_wait), Some(Ok(None)));
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
                .with_context(|| {
                    format!("spawning `{} app-server --listen {listen}`", bin.display())
                })?;

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
        cfg.agents.codex.enabled && self.codex_available
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

/// Probe `<bin> app-server --help` and check whether the `--listen` flag
/// is documented. If yes, the daemon can run a single shared websocket
/// app-server; if no (older codex), we fall back to per-stream stdio. On
/// any failure (binary missing, exec error, garbled output) makes that
/// candidate unavailable. If no candidate can be spawned, we keep `Stdio` as
/// the fallback mode but report codex unavailable.
async fn detect_codex(bin: &str) -> CodexDetection {
    let fallback_bin = PathBuf::from(bin);
    let candidates = {
        let resolved = program_candidates(Path::new(bin));
        if resolved.is_empty() {
            vec![fallback_bin.clone()]
        } else {
            resolved
        }
    };

    for candidate in candidates {
        let output = match tokio::time::timeout(
            Duration::from_secs(5),
            Command::new(&candidate)
                .arg("app-server")
                .arg("--help")
                .output(),
        )
        .await
        {
            Ok(Ok(out)) => out,
            Ok(Err(err)) => {
                warn!(
                    error = %err,
                    configured_bin = %bin,
                    bin = %candidate.display(),
                    "codex app-server --help failed"
                );
                continue;
            }
            Err(_) => {
                warn!(
                    configured_bin = %bin,
                    bin = %candidate.display(),
                    "codex app-server --help timed out"
                );
                continue;
            }
        };
        if !output.status.success() {
            warn!(
                status = %output.status,
                configured_bin = %bin,
                bin = %candidate.display(),
                "codex app-server --help exited unsuccessfully"
            );
            continue;
        }
        let mut help = String::from_utf8_lossy(&output.stdout).into_owned();
        help.push_str(&String::from_utf8_lossy(&output.stderr));
        let mode = if help.contains("--listen") {
            CodexMode::Websocket
        } else {
            CodexMode::Stdio
        };
        return CodexDetection {
            mode,
            bin: candidate,
            available: true,
        };
    }

    CodexDetection {
        mode: CodexMode::Stdio,
        bin: fallback_bin,
        available: false,
    }
}

fn program_candidates(program: &Path) -> Vec<PathBuf> {
    if program.is_absolute() || program.components().count() > 1 {
        return vec![program.to_path_buf()];
    }

    #[cfg(windows)]
    {
        let candidates = match which::which_all(program) {
            Ok(candidates) => candidates
                .filter(|path| {
                    matches!(
                        path.extension()
                            .and_then(|ext| ext.to_str())
                            .map(|ext| ext.to_ascii_lowercase())
                            .as_deref(),
                        Some("exe" | "cmd" | "bat" | "com")
                    )
                })
                .collect::<Vec<_>>(),
            Err(_) => Vec::new(),
        };
        if !candidates.is_empty() {
            return candidates;
        }
    }

    which::which(program)
        .map(|path| vec![path])
        .unwrap_or_default()
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
