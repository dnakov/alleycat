mod agents;
mod cli;
mod config;
mod daemon;
mod framing;
mod host;
mod ipc;
mod paths;
mod protocol;
mod service;
mod state;
mod stream;
#[cfg(test)]
mod test_support;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "alleycat")]
#[command(version, about = "Iroh-backed Alleycat bridge for remote agents")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the long-running daemon. Owns the iroh endpoint, the persistent
    /// identity, and the IPC control socket.
    Serve,
    /// Install the OS-appropriate autostart entry (launchd / systemd-user /
    /// Windows Startup folder). Does not require admin.
    Install,
    /// Remove the autostart entry.
    Uninstall,
    /// Print the running daemon's status (falls back to file-only when the
    /// daemon isn't running).
    Status(cli::status::StatusArgs),
    /// Print the stable pair payload, optionally with an ASCII QR code.
    Pair(cli::pair::PairArgs),
    /// Mint a fresh token. Node id is preserved.
    Rotate,
    /// Tail the daemon log files under `paths::log_dir()`.
    Logs(cli::logs::LogsArgs),
    /// Stop the running daemon.
    Stop,
    /// Reload `host.toml` live in the running daemon.
    Reload,
    /// Inspect agents.
    Agents(cli::agents::AgentsArgs),
    /// Connect to the daemon over iroh like a phone client and run JSON-RPC
    /// methods directly. Defaults to invoking `thread/list` on the chosen agent.
    Probe(cli::probe::ProbeArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Serve => daemon::run().await,
        Command::Install => {
            init_cli_logging();
            service::install()?;
            println!("installed.");
            Ok(())
        }
        Command::Uninstall => {
            init_cli_logging();
            service::uninstall()?;
            println!("uninstalled.");
            Ok(())
        }
        Command::Status(args) => {
            init_cli_logging();
            cli::status::run(args).await
        }
        Command::Pair(args) => {
            init_cli_logging();
            cli::pair::run(args).await
        }
        Command::Rotate => {
            init_cli_logging();
            cli::rotate::run().await
        }
        Command::Logs(args) => {
            init_cli_logging();
            cli::logs::run(args).await
        }
        Command::Stop => {
            init_cli_logging();
            cli::stop::run().await
        }
        Command::Reload => {
            init_cli_logging();
            cli::reload::run().await
        }
        Command::Agents(args) => {
            init_cli_logging();
            cli::agents::run(args).await
        }
        Command::Probe(args) => {
            init_cli_logging();
            cli::probe::run(args).await
        }
    }
}

fn init_cli_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("warn,alleycat=info")),
        )
        .with_writer(std::io::stderr)
        .try_init();
}
