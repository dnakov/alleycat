use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use alleycat_opencode_bridge::OpencodeBridge;
use alleycat_opencode_bridge::opencode_proc::OpencodeRuntime;
use alleycat_pi_bridge::handlers;
use alleycat_pi_bridge::index::ThreadIndex;
use alleycat_pi_bridge::pool::PiPool;
use alleycat_pi_bridge::state::ThreadIndexHandle;
use anyhow::{Context, anyhow};
use tokio::net::TcpStream;
use tokio::sync::OnceCell;

use crate::config::HostConfig;
use crate::protocol::{AgentInfo, AgentWire};
use crate::stream::IrohStream;

#[derive(Clone)]
pub struct AgentManager {
    config: Arc<HostConfig>,
    pi_pool: Arc<PiPool>,
    pi_thread_index: Arc<dyn ThreadIndexHandle>,
    codex_home: PathBuf,
    opencode_bridge: Arc<OnceCell<Arc<OpencodeBridge>>>,
}

impl AgentManager {
    pub async fn new(config: HostConfig) -> anyhow::Result<Self> {
        let codex_home = handlers::lifecycle::default_codex_home();
        tokio::fs::create_dir_all(&codex_home)
            .await
            .with_context(|| format!("creating {}", codex_home.display()))?;
        let pi_pool = Arc::new(PiPool::new(config.agents.pi.bin.clone()));
        let pi_thread_index: Arc<dyn ThreadIndexHandle> =
            ThreadIndex::open_and_hydrate(&codex_home)
                .await
                .context("opening pi thread index")?;

        Ok(Self {
            config: Arc::new(config),
            pi_pool,
            pi_thread_index,
            codex_home,
            opencode_bridge: Arc::new(OnceCell::new()),
        })
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
        ]
    }

    pub async fn serve_agent(&self, agent: &str, stream: IrohStream) -> anyhow::Result<()> {
        match agent {
            "codex" => self.serve_codex(stream).await,
            "pi" => self.serve_pi(stream).await,
            "opencode" => self.serve_opencode(stream).await,
            other => Err(anyhow!("unknown agent `{other}`")),
        }
    }

    pub fn agent_enabled(&self, agent: &str) -> bool {
        match agent {
            "codex" => self.config.agents.codex.enabled,
            "pi" => self.config.agents.pi.enabled,
            "opencode" => self.config.agents.opencode.enabled,
            _ => false,
        }
    }

    async fn serve_codex(&self, mut iroh_stream: IrohStream) -> anyhow::Result<()> {
        let cfg = &self.config.agents.codex;
        if !cfg.enabled {
            return Err(anyhow!("codex agent is disabled"));
        }
        let mut tcp = TcpStream::connect((cfg.host.as_str(), cfg.port))
            .await
            .with_context(|| format!("connecting to Codex at {}:{}", cfg.host, cfg.port))?;
        let _ = tokio::io::copy_bidirectional(&mut iroh_stream, &mut tcp).await;
        Ok(())
    }

    async fn serve_pi(&self, stream: IrohStream) -> anyhow::Result<()> {
        if !self.config.agents.pi.enabled {
            return Err(anyhow!("pi agent is disabled"));
        }
        let (reader, writer) = stream.split();
        alleycat_pi_bridge::run_connection(
            reader,
            writer,
            Arc::clone(&self.pi_pool),
            Arc::clone(&self.pi_thread_index),
            self.codex_home.clone(),
        )
        .await
        .context("serving pi bridge stream")
    }

    async fn serve_opencode(&self, stream: IrohStream) -> anyhow::Result<()> {
        if !self.config.agents.opencode.enabled {
            return Err(anyhow!("opencode agent is disabled"));
        }
        let bridge = self
            .opencode_bridge
            .get_or_try_init(|| async {
                unsafe {
                    std::env::set_var("OPENCODE_BRIDGE_BIN", &self.config.agents.opencode.bin);
                }
                let runtime = OpencodeRuntime::start_from_env()
                    .await
                    .context("starting opencode runtime")?;
                OpencodeBridge::new(runtime)
                    .await
                    .map(Arc::new)
                    .context("initializing opencode bridge")
            })
            .await?;
        alleycat_bridge_core::server::serve_stream(Arc::clone(bridge), stream)
            .await
            .context("serving opencode bridge stream")
    }

    async fn codex_available(&self) -> bool {
        let cfg = &self.config.agents.codex;
        if !cfg.enabled {
            return false;
        }
        tokio::time::timeout(
            Duration::from_millis(250),
            TcpStream::connect((cfg.host.as_str(), cfg.port)),
        )
        .await
        .is_ok_and(|result| result.is_ok())
    }

    fn pi_available(&self) -> bool {
        self.config.agents.pi.enabled && which::which(&self.config.agents.pi.bin).is_ok()
    }

    fn opencode_available(&self) -> bool {
        self.config.agents.opencode.enabled
            && (std::env::var_os("OPENCODE_BRIDGE_BACKEND_URL").is_some()
                || which::which(&self.config.agents.opencode.bin).is_ok())
    }
}
