mod agents;
mod config;
mod framing;
mod host;
mod paths;
mod protocol;
mod state;
mod stream;

use clap::{Parser, Subcommand};
use protocol::PairPayload;
use qrcodegen::{QrCode, QrCodeEcc};
use sha2::{Digest, Sha256};
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
    /// Start the iroh host and serve Codex/Pi/OpenCode streams.
    Serve,
    /// Print the stable pair payload, optionally with an ASCII QR code.
    Pair {
        #[arg(long)]
        qr: bool,
    },
    /// Rotate the stable pairing token without changing the node id.
    Rotate,
    /// Print local host identity, token fingerprint, and agent availability.
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging();
    match Cli::parse().command {
        Command::Serve => host::serve().await,
        Command::Pair { qr } => pair(qr).await,
        Command::Rotate => rotate().await,
        Command::Status => status().await,
    }
}

async fn pair(qr: bool) -> anyhow::Result<()> {
    let config = config::load_or_init().await?;
    let secret_key = state::load_or_create_secret_key().await?;
    let payload = host::pair_payload(&secret_key, &config);
    let json = serde_json::to_string(&payload)?;
    println!("{json}");
    if qr {
        println!();
        print_qr(&json)?;
    }
    Ok(())
}

async fn rotate() -> anyhow::Result<()> {
    let config = config::rotate_token().await?;
    let secret_key = state::load_or_create_secret_key().await?;
    let payload = host::pair_payload(&secret_key, &config);
    println!("{}", serde_json::to_string(&payload)?);
    Ok(())
}

async fn status() -> anyhow::Result<()> {
    let config = config::load_or_init().await?;
    let secret_key = state::load_or_create_secret_key().await?;
    let payload: PairPayload = host::pair_payload(&secret_key, &config);
    let token_fingerprint = token_fingerprint(&config.token);
    println!("node_id: {}", payload.node_id);
    println!(
        "relay: {}",
        payload.relay.as_deref().unwrap_or("<iroh default>")
    );
    println!("token_sha256: {token_fingerprint}");
    println!("config: {}", paths::host_config_file()?.display());
    let agents = agents::AgentManager::new(config).await?;
    println!("agents:");
    for agent in agents.list_agents().await {
        println!(
            "  {} display=\"{}\" wire={} available={}",
            agent.name,
            agent.display_name,
            agent.wire.as_str(),
            agent.available
        );
    }
    Ok(())
}

fn print_qr(data: &str) -> anyhow::Result<()> {
    let code = QrCode::encode_text(data, QrCodeEcc::Medium)
        .map_err(|err| anyhow::anyhow!("encoding QR: {err:?}"))?;
    let border = 2;
    for y in -border..code.size() + border {
        for x in -border..code.size() + border {
            let dark = code.get_module(x, y);
            print!("{}", if dark { "██" } else { "  " });
        }
        println!();
    }
    Ok(())
}

fn token_fingerprint(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    hex::encode(&digest[..8])
}

fn init_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_short_hex() {
        let fp = token_fingerprint("deadbeef");
        assert_eq!(fp.len(), 16);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
