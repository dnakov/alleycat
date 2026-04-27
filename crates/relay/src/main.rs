use std::path::PathBuf;

use alleycat::RelayConfig;
use alleycat::cli;
use alleycat::daemon;
use alleycat::pair::{PairOptions, run_pair};
use alleycat::service;
use alleycat_protocol::Target;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "alleycat")]
#[command(version, about = "Alleycat QUIC relay")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the long-running daemon process. Owns the persistent identity,
    /// the QUIC relay, and the control IPC socket.
    Run,
    /// Install the OS-appropriate autostart entry (launchd / systemd-user
    /// / Windows Startup folder). Does not require admin.
    Install,
    /// Remove the autostart entry.
    Uninstall,
    /// Print the running daemon's status.
    Status(cli::status::StatusArgs),
    /// Print the QR / connect-params JSON the running daemon would hand to
    /// a phone.
    Qr(cli::qr::QrArgs),
    /// Rotate the daemon's cert and token. Existing sessions drop within 5s.
    Rotate,
    /// Manage the allowlist via the running daemon.
    Allow(cli::allow::AllowArgs),
    /// Tail the daemon log files under `paths::log_dir()`.
    Logs(cli::logs::LogsArgs),
    /// Stop the running daemon.
    Stop,
    /// Reload `config.toml` live in the running daemon.
    Reload,
    /// Legacy one-shot relay (no persistent identity, no daemon).
    Relay {
        #[arg(long, default_value = "0.0.0.0")]
        bind: String,
        #[arg(long, default_value_t = 0)]
        udp_port: u16,
        #[arg(long)]
        ready_file: PathBuf,
        #[arg(long = "allow-tcp", value_parser = parse_allow_tcp)]
        allow_tcp: Vec<Target>,
        #[arg(long = "allow-unix")]
        allow_unix: Vec<String>,
    },
    /// Legacy one-shot pair (no persistent identity, prints QR + serves
    /// once).
    Pair {
        #[arg(long, default_value = "0.0.0.0")]
        bind: String,
        #[arg(long, default_value_t = 0)]
        udp_port: u16,
        #[arg(long = "allow-tcp", value_parser = parse_allow_tcp)]
        allow_tcp: Vec<Target>,
        #[arg(long = "allow-unix")]
        allow_unix: Vec<String>,
        /// Override auto-detected host candidates. Repeat for multiple
        /// candidates the phone should try in order
        /// (e.g. `--host studio.tailnet.ts.net --host 192.168.1.5`).
        #[arg(long = "host")]
        host: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run => {
            // Daemon-side logging is initialized inside `daemon::run` so it
            // can route to a rotating file. Don't install a separate
            // subscriber here.
            daemon::run().await
        }
        Command::Install => {
            service::install()?;
            println!("installed.");
            Ok(())
        }
        Command::Uninstall => {
            service::uninstall()?;
            println!("uninstalled.");
            Ok(())
        }
        Command::Status(args) => {
            init_cli_logging();
            cli::status::run(args).await
        }
        Command::Qr(args) => {
            init_cli_logging();
            cli::qr::run(args).await
        }
        Command::Rotate => {
            init_cli_logging();
            cli::rotate::run().await
        }
        Command::Allow(args) => {
            init_cli_logging();
            cli::allow::run(args).await
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
        Command::Relay {
            bind,
            udp_port,
            ready_file,
            allow_tcp,
            allow_unix,
        } => {
            init_cli_logging();
            let mut allowlist = allow_tcp;
            allowlist.extend(allow_unix.into_iter().map(|path| Target::Unix { path }));
            let relay = alleycat::RelayRuntime::bind(RelayConfig {
                bind,
                udp_port,
                ready_file: Some(ready_file),
                allowlist,
            })
            .await?;
            relay.serve().await?;
            Ok(())
        }
        Command::Pair {
            bind,
            udp_port,
            allow_tcp,
            allow_unix,
            host,
        } => {
            init_cli_logging();
            run_pair(PairOptions {
                bind,
                udp_port,
                allow_tcp,
                allow_unix,
                host_overrides: host,
            })
            .await
        }
    }
}

fn init_cli_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,quinn=warn")),
        )
        .try_init();
}

fn parse_allow_tcp(value: &str) -> Result<Target, String> {
    alleycat::config::parse_tcp_target(value)
}
