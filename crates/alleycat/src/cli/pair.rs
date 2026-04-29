use clap::Args;
use qrcodegen::{QrCode, QrCodeEcc};

use crate::cli;
use crate::daemon::control::Request;
use crate::host;
use crate::ipc;
use crate::protocol::PairPayload;

#[derive(Args, Debug)]
pub struct PairArgs {
    /// Render an ASCII QR code for the pair payload.
    #[arg(long)]
    pub qr: bool,
}

pub async fn run(args: PairArgs) -> anyhow::Result<()> {
    let payload: PairPayload = if ipc::is_daemon_running().await {
        let resp = cli::send(Request::Pair).await?;
        cli::decode_data(resp)?
    } else {
        let cfg = crate::config::load_or_init().await?;
        let secret_key = crate::state::load_or_create_secret_key().await?;
        host::pair_payload(&secret_key, &cfg)
    };

    let json = serde_json::to_string(&payload)?;
    println!("{json}");
    if args.qr {
        println!();
        print_qr(&json)?;
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
