use std::path::PathBuf;

use alleycat::RelayConfig;
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,quinn=warn")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Relay {
            bind,
            udp_port,
            ready_file,
            allow_tcp,
            allow_unix,
        } => {
            let mut allowlist = allow_tcp;
            allowlist.extend(allow_unix.into_iter().map(|path| Target::Unix { path }));
            let relay = alleycat::RelayRuntime::bind(RelayConfig {
                bind,
                udp_port,
                ready_file,
                allowlist,
            })
            .await?;
            relay.serve().await?;
        }
    }
    Ok(())
}

fn parse_allow_tcp(value: &str) -> Result<Target, String> {
    let (host, port) = value
        .rsplit_once(':')
        .ok_or_else(|| "expected host:port".to_string())?;
    let port = port
        .parse::<u16>()
        .map_err(|error| format!("invalid port: {error}"))?;
    Ok(Target::Tcp {
        host: host.to_string(),
        port,
    })
}
