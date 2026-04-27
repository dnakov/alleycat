//! Persistent daemon identity: cert, key, token, last bound port, plus a
//! single-instance lock file. Files live under [`crate::paths::state_dir`].
//!
//! Layout:
//! - `cert.pem`   PEM x509 cert with SAN `alleycat.invalid`
//! - `key.pem`    PEM PKCS#8 private key (paired with cert.pem)
//! - `token`      32 hex chars (16 random bytes), trailing newline
//! - `port`       last bound UDP port, plain text decimal
//! - `lock`       empty marker file used for fd-lock single-instance enforcement
//!
//! All writes are atomic (write-tmp, fsync, rename). Unix permissions are
//! tightened to 0600 after rename. Windows hardening is left as a TODO —
//! `%LOCALAPPDATA%` is already user-scoped, but per-file DACLs via
//! `windows-sys` `SECURITY_ATTRIBUTES` should land before v1 ships if the
//! threat model needs it.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use rand::RngCore;
use rcgen::generate_simple_self_signed;
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::paths;

const CERT_SAN: &str = "alleycat.invalid";
const TOKEN_BYTES: usize = 16;

/// Loaded daemon identity. `cert_der` and `key_der` are ready to hand to
/// `rustls::ServerConfig::with_single_cert` / Quinn.
#[derive(Debug, Clone)]
pub struct Identity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
    pub token: String,
    pub last_port: Option<u16>,
}

impl Identity {
    /// Lowercase hex SHA-256 of the DER cert — matches the existing fingerprint
    /// shape produced in `RelayRuntime::bind` so QR + ReadyFile stay stable.
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(&self.cert_der))
    }

    /// Generate a fresh ephemeral identity in memory (no disk I/O). Used by
    /// the legacy one-shot `relay`/`pair` subcommands.
    pub fn generate() -> anyhow::Result<Self> {
        let cert = generate_simple_self_signed(vec![CERT_SAN.to_string()])
            .context("generating self-signed cert")?;
        Ok(Self {
            cert_der: cert.cert.der().to_vec(),
            key_der: cert.key_pair.serialize_der(),
            token: random_token_hex(),
            last_port: None,
        })
    }
}

/// Load identity from disk, or generate a fresh one (and persist it) if no
/// cert/key is on disk yet.
pub async fn load_or_init() -> anyhow::Result<Identity> {
    let cert_path = paths::cert_file()?;
    let key_path = paths::key_file()?;
    let token_path = paths::token_file()?;
    let port_path = paths::port_file()?;

    if cert_path.exists() && key_path.exists() && token_path.exists() {
        let cert_der = read_cert_der(&cert_path).await?;
        let key_der = read_key_der(&key_path).await?;
        let token = read_token(&token_path).await?;
        let last_port = read_port(&port_path).await?;
        return Ok(Identity {
            cert_der,
            key_der,
            token,
            last_port,
        });
    }

    // Mint a fresh identity. Don't clobber port file if it exists already
    // (might be left over from a previous install).
    let identity = mint_and_persist().await?;
    let last_port = read_port(&port_path).await.unwrap_or(None);
    Ok(Identity {
        last_port,
        ..identity
    })
}

/// Write the last-bound UDP port atomically.
pub async fn save_port(port: u16) -> anyhow::Result<()> {
    let path = paths::port_file()?;
    atomic_write(&path, format!("{port}\n").as_bytes()).await?;
    set_mode_0600(&path)?;
    Ok(())
}

/// Generate a fresh cert + token, persist atomically, return new `Identity`.
/// The previous `port` file (if any) is preserved so the daemon can keep
/// the same UDP port across rotations — the QR's port stays valid.
pub async fn rotate() -> anyhow::Result<Identity> {
    let port_path = paths::port_file()?;
    let last_port = read_port(&port_path).await.unwrap_or(None);
    let mut identity = mint_and_persist().await?;
    identity.last_port = last_port;
    Ok(identity)
}

/// Acquire (or create) the single-instance lock file. The caller is
/// responsible for taking the actual write lock via `try_write()` and holding
/// the returned `RwLock` for the daemon's lifetime — dropping it releases.
///
/// Returns the unlocked `fd_lock::RwLock` over the open file handle. This
/// signature is what task #3 specifies; the caller does:
///
/// ```ignore
/// let mut lock = state::acquire_lock().await?;
/// let _guard = lock.try_write().context("another alleycat daemon is running")?;
/// // hold _guard for the daemon's lifetime
/// ```
pub async fn acquire_lock() -> anyhow::Result<fd_lock::RwLock<std::fs::File>> {
    let path = paths::lock_file()?;
    // tokio::fs has no OpenOptions equivalent in stable that returns std::fs::File,
    // so use spawn_blocking to do the open synchronously.
    let lock = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening lock file {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(fd_lock::RwLock::new(file))
    })
    .await
    .context("spawn_blocking join failed")??;
    Ok(lock)
}

async fn mint_and_persist() -> anyhow::Result<Identity> {
    let cert = generate_simple_self_signed(vec![CERT_SAN.to_string()])
        .context("generating self-signed cert")?;
    let cert_pem = cert.cert.pem();
    let key_pem = cert.key_pair.serialize_pem();
    let cert_der = cert.cert.der().to_vec();
    let key_der = cert.key_pair.serialize_der();

    let cert_path = paths::cert_file()?;
    let key_path = paths::key_file()?;
    let token_path = paths::token_file()?;

    atomic_write(&cert_path, cert_pem.as_bytes()).await?;
    set_mode_0600(&cert_path)?;
    atomic_write(&key_path, key_pem.as_bytes()).await?;
    set_mode_0600(&key_path)?;

    let token = random_token_hex();
    atomic_write(&token_path, format!("{token}\n").as_bytes()).await?;
    set_mode_0600(&token_path)?;

    Ok(Identity {
        cert_der,
        key_der,
        token,
        last_port: None,
    })
}

async fn read_cert_der(path: &Path) -> anyhow::Result<Vec<u8>> {
    let data = fs::read(path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    let mut cursor = std::io::Cursor::new(&data);
    let mut iter = rustls_pemfile::certs(&mut cursor);
    let first = iter
        .next()
        .ok_or_else(|| anyhow!("no PEM cert in {}", path.display()))?
        .with_context(|| format!("parsing PEM cert from {}", path.display()))?;
    Ok(first.to_vec())
}

async fn read_key_der(path: &Path) -> anyhow::Result<Vec<u8>> {
    let data = fs::read(path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    let mut cursor = std::io::Cursor::new(&data);
    let key = rustls_pemfile::private_key(&mut cursor)
        .with_context(|| format!("parsing PEM key from {}", path.display()))?
        .ok_or_else(|| anyhow!("no PEM private key in {}", path.display()))?;
    Ok(key.secret_der().to_vec())
}

async fn read_token(path: &Path) -> anyhow::Result<String> {
    let raw = fs::read_to_string(path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("token file {} is empty", path.display()));
    }
    Ok(trimmed.to_string())
}

async fn read_port(path: &Path) -> anyhow::Result<Option<u16>> {
    match fs::read_to_string(path).await {
        Ok(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            let port: u16 = trimmed
                .parse()
                .with_context(|| format!("parsing port from {}", path.display()))?;
            Ok(Some(port))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(anyhow::Error::from(err).context(format!("reading {}", path.display()))),
    }
}

async fn atomic_write(target: &Path, contents: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating parent of {}", target.display()))?;
    }
    let tmp = tmp_path(target);
    {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .await
            .with_context(|| format!("opening {}", tmp.display()))?;
        f.write_all(contents)
            .await
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.flush().await.ok();
        f.sync_all()
            .await
            .with_context(|| format!("fsync {}", tmp.display()))?;
    }
    set_mode_0600(&tmp).ok();
    fs::rename(&tmp, target)
        .await
        .with_context(|| format!("renaming {} -> {}", tmp.display(), target.display()))?;
    // Best-effort fsync the parent so the rename is durable.
    if let Some(parent) = target.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

fn tmp_path(target: &Path) -> PathBuf {
    let mut name = target
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    target.with_file_name(name)
}

fn set_mode_0600(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 0600 {}", path.display()))?;
    }
    #[cfg(windows)]
    {
        // TODO: harden per-file with windows-sys SECURITY_ATTRIBUTES granting
        // only the current user GENERIC_ALL. For v1 we rely on
        // %LOCALAPPDATA% being user-scoped already.
        let _ = path;
    }
    Ok(())
}

fn random_token_hex() -> String {
    let mut buf = [0u8; TOKEN_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    hex::encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempHome;

    #[tokio::test]
    async fn first_load_creates_identity() {
        let _h = TempHome::new();
        let id = load_or_init().await.expect("first load");
        assert_eq!(id.token.len(), 32, "token should be 32 hex chars");
        assert!(!id.cert_der.is_empty());
        assert!(!id.key_der.is_empty());

        // All four files now exist.
        assert!(paths::cert_file().unwrap().exists());
        assert!(paths::key_file().unwrap().exists());
        assert!(paths::token_file().unwrap().exists());

        // Second load returns the same identity.
        let id2 = load_or_init().await.expect("second load");
        assert_eq!(id.token, id2.token);
        assert_eq!(id.cert_der, id2.cert_der);
        assert_eq!(id.key_der, id2.key_der);
    }

    #[tokio::test]
    async fn rotate_changes_token_and_fingerprint() {
        let _h = TempHome::new();
        let id1 = load_or_init().await.expect("first");
        let id2 = rotate().await.expect("rotate");
        assert_ne!(id1.token, id2.token, "rotate must change token");
        assert_ne!(
            id1.fingerprint(),
            id2.fingerprint(),
            "rotate must change fingerprint"
        );

        // Third load should observe the rotated identity.
        let id3 = load_or_init().await.expect("post-rotate load");
        assert_eq!(id3.token, id2.token);
        assert_eq!(id3.fingerprint(), id2.fingerprint());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn persisted_files_are_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let _h = TempHome::new();
        let _ = load_or_init().await.expect("init");
        for path in [
            paths::cert_file().unwrap(),
            paths::key_file().unwrap(),
            paths::token_file().unwrap(),
        ] {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{} should be 0600", path.display());
        }
    }

    #[tokio::test]
    async fn save_port_round_trips() {
        let _h = TempHome::new();
        let _ = load_or_init().await.expect("init");
        save_port(45123).await.expect("save_port");
        let id = load_or_init().await.expect("reload");
        assert_eq!(id.last_port, Some(45123));
    }

    #[tokio::test]
    async fn rotate_preserves_port() {
        let _h = TempHome::new();
        let _ = load_or_init().await.expect("init");
        save_port(60001).await.unwrap();
        let rotated = rotate().await.expect("rotate");
        assert_eq!(rotated.last_port, Some(60001));
    }

    #[tokio::test]
    async fn lock_blocks_second_writer() {
        let _h = TempHome::new();
        let mut a = acquire_lock().await.expect("lock a");
        let _guard_a = a.try_write().expect("first try_write succeeds");

        let mut b = acquire_lock().await.expect("lock b");
        let result = b.try_write();
        assert!(
            result.is_err(),
            "second try_write must fail while first guard is held"
        );
    }
}
