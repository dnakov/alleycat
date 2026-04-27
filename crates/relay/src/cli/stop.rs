use crate::cli;
use crate::daemon::control::Request;

pub async fn run() -> anyhow::Result<()> {
    let resp = cli::send(Request::Stop).await?;
    cli::require_ok(&resp)?;
    println!("daemon stopping.");
    Ok(())
}
