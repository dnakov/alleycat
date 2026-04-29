use std::sync::Arc;

use arc_swap::ArcSwap;
use clap::Args;

use crate::agents::AgentManager;
use crate::cli;
use crate::daemon::control::{Request, StatusInfo, token_fingerprint};
use crate::ipc;
use crate::paths;
use crate::protocol::AgentInfo;

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Emit machine-readable JSON instead of the human summary.
    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: StatusArgs) -> anyhow::Result<()> {
    let info = if ipc::is_daemon_running().await {
        let resp = cli::send(Request::Status).await?;
        cli::decode_data::<StatusInfo>(resp)?
    } else {
        offline_status().await?
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&info)?);
        return Ok(());
    }

    println!("alleycat daemon");
    println!("  pid:               {}", info.pid);
    println!("  node id:           {}", info.node_id);
    println!("  token (sha256/16): {}", info.token_short);
    println!(
        "  relay:             {}",
        info.relay.as_deref().unwrap_or("<iroh default>")
    );
    println!("  config:            {}", info.config_path);
    if info.uptime_secs > 0 {
        println!("  uptime (s):        {}", info.uptime_secs);
    } else {
        println!("  uptime (s):        <daemon not running>");
    }
    println!("  agents:");
    for agent in &info.agents {
        println!(
            "    {} display=\"{}\" wire={} available={}",
            agent.name,
            agent.display_name,
            agent.wire.as_str(),
            agent.available
        );
    }
    Ok(())
}

/// Status when the daemon isn't running. Pid is 0 and uptime is 0 so the
/// human renderer can call out the offline state.
async fn offline_status() -> anyhow::Result<StatusInfo> {
    let cfg = crate::config::load_or_init().await?;
    let secret_key = crate::state::load_or_create_secret_key().await?;
    let agents_cfg = Arc::new(ArcSwap::from_pointee(cfg.clone()));
    let agents = AgentManager::new(Arc::clone(&agents_cfg)).await?;
    let agent_list: Vec<AgentInfo> = agents.list_agents().await;
    Ok(StatusInfo {
        pid: 0,
        node_id: secret_key.public().to_string(),
        token_short: token_fingerprint(&cfg.token),
        relay: cfg.relay.clone(),
        config_path: paths::host_config_file()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "<unknown>".to_string()),
        uptime_secs: 0,
        agents: agent_list,
    })
}
