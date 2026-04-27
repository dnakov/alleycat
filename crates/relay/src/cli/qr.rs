use std::path::PathBuf;

use anyhow::Context;
use clap::Args;
use qrcodegen::{QrCode, QrCodeEcc};

use crate::cli;
use crate::daemon::control::Request;
use crate::pair::{self, ConnectParams};

#[derive(Args, Debug)]
pub struct QrArgs {
    /// Optional path to write a PNG of the QR. v1 may not support image
    /// rendering — in that case the daemon returns an error and the JSON is
    /// still printed to stdout.
    #[arg(long)]
    pub image: Option<PathBuf>,
}

pub async fn run(args: QrArgs) -> anyhow::Result<()> {
    let resp = cli::send(Request::Qr {
        image: args.image.clone(),
    })
    .await?;
    let params: ConnectParams = cli::decode_data(resp)?;

    let json = serde_json::to_string(&params).context("serialize connect params")?;
    let qr = QrCode::encode_text(&json, QrCodeEcc::Medium)
        .context("encode connect params as qr code")?;
    pair::print_qr(&qr);
    println!("{json}");
    eprintln!();
    eprintln!("alleycat: relay host candidates:");
    for (i, h) in params.host_candidates.iter().enumerate() {
        eprintln!("  {}. {h}", i + 1);
    }
    Ok(())
}
