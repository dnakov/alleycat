use clap::{Args, Subcommand};

use crate::cli;
use crate::daemon::control::Request;
use crate::protocol::AgentInfo;

#[derive(Args, Debug)]
pub struct AgentsArgs {
    #[command(subcommand)]
    pub cmd: AgentsCmd,
}

#[derive(Subcommand, Debug)]
pub enum AgentsCmd {
    /// List configured agents and their availability.
    List,
}

pub async fn run(args: AgentsArgs) -> anyhow::Result<()> {
    match args.cmd {
        AgentsCmd::List => {
            let resp = cli::send(Request::AgentsList).await?;
            let agents: Vec<AgentInfo> = cli::decode_data(resp)?;
            for a in &agents {
                println!(
                    "{name}\tdisplay=\"{display}\"\twire={wire}\tavailable={avail}",
                    name = a.name,
                    display = a.display_name,
                    wire = a.wire.as_str(),
                    avail = a.available
                );
            }
            Ok(())
        }
    }
}
