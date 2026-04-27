use alleycat_protocol::Target;
use anyhow::{Context, anyhow};
use clap::{Args, Subcommand};

use crate::cli;
use crate::config;
use crate::daemon::control::Request;

#[derive(Args, Debug)]
pub struct AllowArgs {
    #[command(subcommand)]
    pub cmd: AllowCmd,
}

#[derive(Subcommand, Debug)]
pub enum AllowCmd {
    /// Add a target to the allowlist (`alleycat allow add tcp 127.0.0.1:8390`).
    Add {
        /// `tcp` or `unix`.
        kind: String,
        /// `host:port` for tcp, filesystem path for unix.
        value: String,
    },
    /// Remove a target from the allowlist.
    Rm { kind: String, value: String },
    /// Print the current allowlist.
    List,
}

pub async fn run(args: AllowArgs) -> anyhow::Result<()> {
    match args.cmd {
        AllowCmd::Add { kind, value } => {
            let target = parse_target(&kind, &value)?;
            let resp = cli::send(Request::AllowAdd { target }).await?;
            cli::require_ok(&resp)?;
            println!("added.");
        }
        AllowCmd::Rm { kind, value } => {
            let target = parse_target(&kind, &value)?;
            let resp = cli::send(Request::AllowRm { target }).await?;
            cli::require_ok(&resp)?;
            println!("removed.");
        }
        AllowCmd::List => {
            let resp = cli::send(Request::AllowList).await?;
            let targets: Vec<Target> = cli::decode_data(resp)?;
            if targets.is_empty() {
                println!("(empty)");
            } else {
                for t in &targets {
                    println!("{}", format_target(t));
                }
            }
        }
    }
    Ok(())
}

fn parse_target(kind: &str, value: &str) -> anyhow::Result<Target> {
    match kind {
        "tcp" => config::parse_tcp_target(value)
            .map_err(|e| anyhow!(e))
            .with_context(|| format!("parsing tcp target {value:?}")),
        "unix" => Ok(Target::Unix {
            path: value.to_string(),
        }),
        other => Err(anyhow!(
            "unknown target kind {other:?} (expected `tcp` or `unix`)"
        )),
    }
}

fn format_target(t: &Target) -> String {
    match t {
        Target::Tcp { host, port } => format!("tcp {host}:{port}"),
        Target::Unix { path } => format!("unix {path}"),
    }
}
