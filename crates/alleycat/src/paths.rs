use std::path::PathBuf;

use anyhow::{Context, anyhow};
use directories::BaseDirs;

fn home_dir() -> anyhow::Result<PathBuf> {
    BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .ok_or_else(|| anyhow!("could not determine home directory"))
}

pub fn alleycat_dir() -> anyhow::Result<PathBuf> {
    let path = home_dir()?.join(".alleycat");
    std::fs::create_dir_all(&path).with_context(|| format!("creating {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
    }
    Ok(path)
}

pub fn host_key_file() -> anyhow::Result<PathBuf> {
    Ok(alleycat_dir()?.join("host.key"))
}

pub fn host_config_file() -> anyhow::Result<PathBuf> {
    Ok(alleycat_dir()?.join("host.toml"))
}

pub fn host_lock_file() -> anyhow::Result<PathBuf> {
    Ok(alleycat_dir()?.join("host.lock"))
}
