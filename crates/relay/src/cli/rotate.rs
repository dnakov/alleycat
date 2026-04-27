use crate::cli;
use crate::daemon::control::{Request, RotateResult};

pub async fn run() -> anyhow::Result<()> {
    let resp = cli::send(Request::Rotate).await?;
    let result: RotateResult = cli::decode_data(resp)?;
    println!(
        "rotated. new fingerprint short: {}. existing sessions will drop within 5s.",
        result.fingerprint_short
    );
    Ok(())
}
