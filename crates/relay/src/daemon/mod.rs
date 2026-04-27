//! Long-running daemon process for `alleycat run`. Owns:
//! - The single-instance file lock (held for the daemon's lifetime).
//! - The persistent [`crate::state::Identity`] (cert/key/token reused across
//!   restarts).
//! - A live [`crate::RelayRuntime`] serving QUIC, swappable in place when the
//!   user calls `alleycat rotate`.
//! - The IPC control listener accepting one request per connection from CLI
//!   subcommands like `status` / `allow` / `reload`.
//!
//! The daemon installs SIGTERM/SIGINT handlers and a `tokio::sync::Notify`
//! shutdown signal so a `Stop` request can write its `{ok:true}` response
//! before the runtime is torn down.

pub mod control;
pub mod logging;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alleycat_protocol::{Target, read_frame_json, write_frame_json};
use anyhow::{Context, anyhow};
use tokio::sync::{Mutex, Notify};
use tracing::{debug, error, info, warn};

use crate::daemon::control::{Request, Response, RotateResult, StatusInfo, short_fingerprint};
use crate::ipc::{ControlListener, ControlStream};
use crate::{RelayConfig, RelayRuntime, config, host_detect, paths, state};

/// Entry point for `alleycat run`. Loads config + identity, binds the relay
/// and the control socket, services control requests until SIGTERM / SIGINT
/// or a `Stop` request, then drains and exits.
pub async fn run() -> anyhow::Result<()> {
    let log_dir = paths::log_dir().context("locating log directory")?;
    let cfg = config::load_or_init().await.context("loading config")?;
    let _log_guard = logging::init(&cfg.log.level, &log_dir).context("initializing logging")?;

    let mut lock = state::acquire_lock().await.context("acquiring lock file")?;
    let _lock_guard = lock.try_write().map_err(|_| {
        let pid_hint = read_pid_file().unwrap_or(None);
        match pid_hint {
            Some(pid) => anyhow!(
                "another alleycat daemon is already running (pid {pid}). \
                 use `alleycat stop` first."
            ),
            None => {
                anyhow!("another alleycat daemon is already running. use `alleycat stop` first.")
            }
        }
    })?;

    let pid_path = paths::state_dir()?.join("daemon.pid");
    write_pid_file(&pid_path).context("writing pid file")?;
    let _pid_cleanup = RemoveOnDrop(pid_path);

    let identity = state::load_or_init().await.context("loading identity")?;
    info!(
        fingerprint_short = %short_fingerprint(&identity.fingerprint()),
        "loaded persistent identity"
    );

    let relay_cfg = RelayConfig {
        bind: cfg.relay.bind.clone(),
        udp_port: identity.last_port.unwrap_or(cfg.relay.udp_port),
        ready_file: None,
        allowlist: config::parse_targets(&cfg),
    };
    let relay = RelayRuntime::bind_with_identity(relay_cfg, identity.clone())
        .await
        .context("binding relay runtime")?;
    let bound_port = relay.ready().udp_port;
    state::save_port(bound_port)
        .await
        .with_context(|| format!("persisting bound port {bound_port}"))?;
    info!(port = bound_port, "relay bound");

    let host_candidates = if cfg.host.overrides.is_empty() {
        host_detect::detect_candidates_with(cfg.host.tailscale_bin.as_deref())
    } else {
        cfg.host.overrides.clone()
    };

    let started_at = Instant::now();
    let shutdown = Arc::new(Notify::new());
    let relay = Arc::new(relay);
    let daemon = Arc::new(DaemonState {
        relay: Arc::clone(&relay),
        config: Mutex::new(cfg),
        host_candidates: Mutex::new(host_candidates),
        started_at,
        shutdown: Arc::clone(&shutdown),
    });

    let serve_task = {
        let relay = Arc::clone(&relay);
        tokio::spawn(async move {
            if let Err(error) = relay.serve_shared().await {
                error!("relay serve loop ended: {error:#}");
            }
        })
    };

    let listener = ControlListener::bind()
        .await
        .context("binding control listener")?;
    info!("control socket listening");

    let accept_task = tokio::spawn(accept_loop(Arc::clone(&daemon), listener));

    wait_for_signal(Arc::clone(&shutdown)).await;
    info!("shutdown initiated");

    accept_task.abort();
    let _ = accept_task.await;
    relay.shutdown();
    let _ = serve_task.await;

    info!("daemon exited cleanly");
    Ok(())
}

struct DaemonState {
    relay: Arc<RelayRuntime>,
    config: Mutex<config::Config>,
    host_candidates: Mutex<Vec<String>>,
    started_at: Instant,
    shutdown: Arc<Notify>,
}

async fn accept_loop(daemon: Arc<DaemonState>, mut listener: ControlListener) {
    loop {
        match listener.accept().await {
            Ok(stream) => {
                let daemon = Arc::clone(&daemon);
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(daemon, stream).await {
                        debug!("control connection ended: {error:#}");
                    }
                });
            }
            Err(error) => {
                warn!("control accept error: {error:#}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn handle_connection(
    daemon: Arc<DaemonState>,
    mut stream: Box<dyn ControlStream>,
) -> anyhow::Result<()> {
    let request: Request = read_frame_json(&mut stream)
        .await
        .context("reading request")?;
    debug!(?request, "control request");
    let (response, after) = dispatch(Arc::clone(&daemon), request).await;
    write_frame_json(&mut stream, &response)
        .await
        .context("writing response")?;
    drop(stream);
    if let Some(action) = after {
        match action {
            PostResponse::Shutdown => {
                daemon.shutdown.notify_waiters();
            }
        }
    }
    Ok(())
}

enum PostResponse {
    Shutdown,
}

async fn dispatch(daemon: Arc<DaemonState>, request: Request) -> (Response, Option<PostResponse>) {
    match request {
        Request::Status => (handle_status(&daemon).await, None),
        Request::Qr { image } => (handle_qr(&daemon, image).await, None),
        Request::Rotate => (handle_rotate(&daemon).await, None),
        Request::AllowAdd { target } => (handle_allow_add(&daemon, target).await, None),
        Request::AllowRm { target } => (handle_allow_rm(&daemon, target).await, None),
        Request::AllowList => (handle_allow_list(&daemon).await, None),
        Request::Reload => (handle_reload(&daemon).await, None),
        Request::Stop => (Response::ok(), Some(PostResponse::Shutdown)),
    }
}

async fn handle_status(daemon: &DaemonState) -> Response {
    let ready = daemon.relay.ready();
    let info = StatusInfo {
        pid: std::process::id(),
        udp_port: ready.udp_port,
        fingerprint_short: short_fingerprint(&ready.cert_fingerprint),
        allowlist: daemon.relay.allowlist(),
        host_candidates: daemon.host_candidates.lock().await.clone(),
        uptime_secs: daemon.started_at.elapsed().as_secs(),
    };
    Response::ok_with(&info).unwrap_or_else(|e| Response::err(e.to_string()))
}

async fn handle_qr(daemon: &DaemonState, image: Option<PathBuf>) -> Response {
    if image.is_some() {
        return Response::err("image rendering not yet supported");
    }
    let ready = daemon.relay.ready();
    let host_candidates = daemon.host_candidates.lock().await.clone();
    let params = crate::pair::ConnectParams {
        protocol_version: ready.protocol_version,
        udp_port: ready.udp_port,
        cert_fingerprint: ready.cert_fingerprint.clone(),
        token: ready.token.clone(),
        host_candidates,
    };
    Response::ok_with(&params).unwrap_or_else(|e| Response::err(e.to_string()))
}

async fn handle_rotate(daemon: &DaemonState) -> Response {
    let new_id = match state::rotate().await {
        Ok(v) => v,
        Err(error) => return Response::err(format!("rotate failed: {error:#}")),
    };
    let new_fp = new_id.fingerprint();
    if let Err(error) = daemon
        .relay
        .swap_identity(new_id, Duration::from_secs(5))
        .await
    {
        return Response::err(format!("swap_identity failed: {error:#}"));
    }
    Response::ok_with(&RotateResult {
        fingerprint_short: short_fingerprint(&new_fp),
    })
    .unwrap_or_else(|e| Response::err(e.to_string()))
}

async fn handle_allow_add(daemon: &DaemonState, target: Target) -> Response {
    let mut cfg = daemon.config.lock().await;
    if !push_target(&mut cfg, &target) {
        return Response::ok();
    }
    if let Err(error) = config::save(&cfg).await {
        return Response::err(format!("saving config: {error:#}"));
    }
    let targets = config::parse_targets(&cfg);
    drop(cfg);
    daemon.relay.set_allowlist(targets);
    Response::ok()
}

async fn handle_allow_rm(daemon: &DaemonState, target: Target) -> Response {
    let mut cfg = daemon.config.lock().await;
    if !remove_target(&mut cfg, &target) {
        return Response::ok();
    }
    if let Err(error) = config::save(&cfg).await {
        return Response::err(format!("saving config: {error:#}"));
    }
    let targets = config::parse_targets(&cfg);
    drop(cfg);
    daemon.relay.set_allowlist(targets);
    Response::ok()
}

async fn handle_allow_list(daemon: &DaemonState) -> Response {
    let allowlist = daemon.relay.allowlist();
    Response::ok_with(&allowlist).unwrap_or_else(|e| Response::err(e.to_string()))
}

async fn handle_reload(daemon: &DaemonState) -> Response {
    let new_cfg = match config::load_or_init().await {
        Ok(c) => c,
        Err(error) => return Response::err(format!("loading config: {error:#}")),
    };
    let mut cur = daemon.config.lock().await;
    if let Some(reason) = config::diff_requires_restart(&cur, &new_cfg) {
        return Response::err(reason.to_string());
    }
    let targets = config::parse_targets(&new_cfg);
    let host_candidates = if new_cfg.host.overrides.is_empty() {
        host_detect::detect_candidates_with(new_cfg.host.tailscale_bin.as_deref())
    } else {
        new_cfg.host.overrides.clone()
    };
    *cur = new_cfg;
    drop(cur);
    *daemon.host_candidates.lock().await = host_candidates;
    daemon.relay.set_allowlist(targets);
    Response::ok()
}

fn push_target(cfg: &mut config::Config, target: &Target) -> bool {
    match target {
        Target::Tcp { host, port } => {
            let s = format!("{host}:{port}");
            if cfg.allowlist.tcp.iter().any(|e| e == &s) {
                false
            } else {
                cfg.allowlist.tcp.push(s);
                true
            }
        }
        Target::Unix { path } => {
            if cfg.allowlist.unix.iter().any(|e| e == path) {
                false
            } else {
                cfg.allowlist.unix.push(path.clone());
                true
            }
        }
    }
}

fn remove_target(cfg: &mut config::Config, target: &Target) -> bool {
    match target {
        Target::Tcp { host, port } => {
            let s = format!("{host}:{port}");
            let before = cfg.allowlist.tcp.len();
            cfg.allowlist.tcp.retain(|e| e != &s);
            cfg.allowlist.tcp.len() != before
        }
        Target::Unix { path } => {
            let before = cfg.allowlist.unix.len();
            cfg.allowlist.unix.retain(|e| e != path);
            cfg.allowlist.unix.len() != before
        }
    }
}

async fn wait_for_signal(shutdown: Arc<Notify>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = term.recv() => info!("received SIGTERM"),
            _ = int.recv() => info!("received SIGINT"),
            _ = shutdown.notified() => info!("received Stop request"),
        }
    }
    #[cfg(windows)]
    {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => info!("received Ctrl-C"),
            _ = shutdown.notified() => info!("received Stop request"),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        shutdown.notified().await;
    }
}

fn write_pid_file(path: &std::path::Path) -> anyhow::Result<()> {
    std::fs::write(path, format!("{}\n", std::process::id()))
        .with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn read_pid_file() -> anyhow::Result<Option<u32>> {
    let path = paths::state_dir()?.join("daemon.pid");
    match std::fs::read_to_string(&path) {
        Ok(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            Ok(trimmed.parse::<u32>().ok())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

struct RemoveOnDrop(PathBuf);
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
