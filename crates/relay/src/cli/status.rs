use alleycat_protocol::Target;
use clap::Args;

use crate::cli;
use crate::daemon::control::{Request, StatusInfo};

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Emit machine-readable JSON instead of the human summary.
    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: StatusArgs) -> anyhow::Result<()> {
    let resp = cli::send(Request::Status).await?;
    let info: StatusInfo = cli::decode_data(resp)?;

    if args.json {
        let json = serde_json::to_string_pretty(&info)?;
        println!("{json}");
        return Ok(());
    }

    println!("alleycat daemon");
    println!("  pid:                {}", info.pid);
    println!("  udp port:           {}", info.udp_port);
    println!("  fingerprint (16):   {}", info.fingerprint_short);
    println!("  uptime (s):         {}", info.uptime_secs);
    println!("  host candidates:");
    if info.host_candidates.is_empty() {
        println!("    (none)");
    } else {
        for h in &info.host_candidates {
            println!("    - {h}");
        }
    }
    println!("  allowlist:");
    if info.allowlist.is_empty() {
        println!("    (empty)");
    } else {
        for t in &info.allowlist {
            println!("    - {}", format_target(t));
        }
    }
    Ok(())
}

fn format_target(t: &Target) -> String {
    match t {
        Target::Tcp { host, port } => format!("tcp {host}:{port}"),
        Target::Unix { path } => format!("unix {path}"),
    }
}
