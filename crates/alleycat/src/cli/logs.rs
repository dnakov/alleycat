//! `alleycat logs` — read daemon log files directly from disk. Does not go
//! through the daemon; the log file is just a file under `paths::log_dir()`.
//! `--follow` polls at 200ms; deliberately avoids the `notify` crate.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, anyhow};
use clap::Args;
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};

use crate::paths;

#[derive(Args, Debug)]
pub struct LogsArgs {
    /// Tail the log file, polling for new lines every 200ms.
    #[arg(short, long)]
    pub follow: bool,
    /// Print only the last N lines before exit / before following.
    #[arg(short = 'n', long, default_value_t = 200)]
    pub lines: usize,
}

pub async fn run(args: LogsArgs) -> anyhow::Result<()> {
    let dir = paths::log_dir().context("locating log directory")?;
    let path = latest_log_file(&dir)
        .await?
        .ok_or_else(|| anyhow!("no daemon log files in {}", dir.display()))?;

    let initial = fs::read_to_string(&path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    let tail: Vec<&str> = initial
        .lines()
        .rev()
        .take(args.lines)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    for line in &tail {
        println!("{line}");
    }

    if !args.follow {
        return Ok(());
    }

    let mut current_path = path;
    let mut file = fs::File::open(&current_path).await?;
    let mut offset = file.metadata().await?.len();
    file.seek(SeekFrom::Start(offset)).await?;
    let mut reader = BufReader::new(file);

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) => {
                tokio::time::sleep(Duration::from_millis(200)).await;
                if let Some(next) = latest_log_file(&dir).await?
                    && next != current_path
                {
                    current_path = next;
                    let f = fs::File::open(&current_path).await?;
                    offset = 0;
                    reader = BufReader::new(f);
                    continue;
                }
                let metadata = fs::metadata(&current_path).await?;
                if metadata.len() < offset {
                    let f = fs::File::open(&current_path).await?;
                    offset = 0;
                    reader = BufReader::new(f);
                }
            }
            Ok(n) => {
                offset += n as u64;
                print!("{line}");
            }
            Err(error) => return Err(error.into()),
        }
    }
}

async fn latest_log_file(dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    let mut entries = match fs::read_dir(dir).await {
        Ok(e) => e,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut latest: Option<(String, PathBuf)> = None;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();
        if !name_str.starts_with("daemon.log") {
            continue;
        }
        let path = entry.path();
        match &latest {
            None => latest = Some((name_str, path)),
            Some((cur, _)) if name_str > *cur => latest = Some((name_str, path)),
            _ => {}
        }
    }
    Ok(latest.map(|(_, p)| p))
}
