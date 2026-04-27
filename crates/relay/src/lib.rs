pub mod cli;
pub mod config;
pub mod daemon;
pub mod host_detect;
pub mod ipc;
pub mod pair;
pub mod paths;
pub mod service;
pub mod state;

#[cfg(test)]
mod test_support;

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use alleycat_protocol::{
    ConnectAck, ConnectHello, OpenRequest, OpenResponse, PROTOCOL_VERSION, ReadyFile, Target,
    read_frame_json, write_frame_json,
};
use anyhow::{Context, anyhow};
use arc_swap::ArcSwap;
use pin_project_lite::pin_project;
use quinn::{Endpoint, RecvStream, SendStream};
use rand::RngCore;
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpStream, UnixStream};
use tracing::debug;

pub use crate::state::Identity;

#[derive(Debug, Clone)]
pub struct RelayConfig {
    pub bind: String,
    pub udp_port: u16,
    pub ready_file: Option<PathBuf>,
    pub allowlist: Vec<Target>,
}

pub struct RelayRuntime {
    endpoint: Endpoint,
    ready: Arc<ArcSwap<ReadyFile>>,
    allowlist: Arc<ArcSwap<Vec<Target>>>,
    accepted_tokens: Arc<ArcSwap<Vec<String>>>,
}

pin_project! {
    struct RelayStream {
        #[pin]
        send: SendStream,
        #[pin]
        recv: RecvStream,
    }
}

impl RelayRuntime {
    /// Bind a relay with a freshly generated ephemeral identity. Used by the
    /// legacy one-shot `relay`/`pair` subcommands.
    pub async fn bind(config: RelayConfig) -> anyhow::Result<Self> {
        let identity = Identity::generate()?;
        Self::bind_with_identity(config, identity).await
    }

    /// Bind a relay with a caller-supplied identity. The daemon uses this to
    /// reuse cert/key/token across restarts (see `state.rs`).
    pub async fn bind_with_identity(
        config: RelayConfig,
        identity: Identity,
    ) -> anyhow::Result<Self> {
        let fingerprint = hex::encode(Sha256::digest(&identity.cert_der));
        let server_config = build_server_config(&identity)?;

        let addr = resolve_bind_addr(&config.bind, config.udp_port)?;
        let endpoint = Endpoint::server(server_config, addr)?;
        let local_port = endpoint.local_addr()?.port();
        let ready = ReadyFile {
            protocol_version: PROTOCOL_VERSION,
            udp_port: local_port,
            cert_fingerprint: fingerprint,
            token: identity.token.clone(),
            pid: std::process::id(),
            allowlist: config.allowlist.clone(),
        };
        if let Some(path) = config.ready_file.as_deref() {
            write_ready_file(path, &ready).await?;
        }
        Ok(Self {
            endpoint,
            ready: Arc::new(ArcSwap::from_pointee(ready)),
            allowlist: Arc::new(ArcSwap::from_pointee(config.allowlist)),
            accepted_tokens: Arc::new(ArcSwap::from_pointee(vec![identity.token])),
        })
    }

    /// Snapshot of the current `ReadyFile`. Cheap to clone — backed by
    /// an `Arc` internally. The daemon takes a fresh snapshot each time it
    /// renders a status/QR response so it sees post-rotate values.
    pub fn ready(&self) -> Arc<ReadyFile> {
        self.ready.load_full()
    }

    /// Replace the active allowlist atomically. New connections see the new
    /// list; in-flight `OpenRequest`s already past the check are unaffected.
    pub fn set_allowlist(&self, allowlist: Vec<Target>) {
        self.allowlist.store(Arc::new(allowlist));
    }

    pub fn allowlist(&self) -> Vec<Target> {
        self.allowlist.load().as_ref().clone()
    }

    /// Swap to a new identity. During `drain` both the previous and the new
    /// tokens are accepted so in-flight handshakes finish; after `drain`
    /// elapses the previous token is removed from the accepted set.
    ///
    /// Takes `&self` (not `&mut self`) so the daemon can call this from a
    /// control-socket handler while the accept loop is running concurrently
    /// against the same `Arc<RelayRuntime>`. All mutated fields use interior
    /// mutability (`ArcSwap` for `ready`/`accepted_tokens`; Quinn's
    /// `Endpoint::set_server_config` already takes `&self`).
    pub async fn swap_identity(&self, identity: Identity, drain: Duration) -> anyhow::Result<()> {
        let new_server_config = build_server_config(&identity)?;
        self.endpoint.set_server_config(Some(new_server_config));

        let fingerprint = hex::encode(Sha256::digest(&identity.cert_der));
        let mut new_ready = self.ready.load().as_ref().clone();
        new_ready.cert_fingerprint = fingerprint;
        new_ready.token = identity.token.clone();
        self.ready.store(Arc::new(new_ready));

        let mut combined = self.accepted_tokens.load().as_ref().clone();
        if !combined.contains(&identity.token) {
            combined.push(identity.token.clone());
        }
        self.accepted_tokens.store(Arc::new(combined));

        let accepted = Arc::clone(&self.accepted_tokens);
        let new_token = identity.token;
        tokio::spawn(async move {
            tokio::time::sleep(drain).await;
            let pruned: Vec<String> = accepted
                .load()
                .iter()
                .filter(|t| t.as_str() == new_token.as_str())
                .cloned()
                .collect();
            accepted.store(Arc::new(pruned));
        });
        Ok(())
    }

    /// Run the QUIC accept loop. Consumes the runtime; appropriate for the
    /// legacy one-shot `relay`/`pair` paths where nothing else holds a
    /// reference. The daemon should use [`serve_shared`] instead so it can
    /// keep handing the same runtime to control-socket handlers.
    pub async fn serve(self) -> anyhow::Result<()> {
        Arc::new(self).serve_shared().await
    }

    /// Run the QUIC accept loop against a shared `Arc<Self>`. The daemon
    /// holds the same `Arc` in its control-socket handlers so it can call
    /// [`Self::set_allowlist`] / [`Self::swap_identity`] / [`Self::ready`]
    /// concurrently with the accept loop.
    pub async fn serve_shared(self: Arc<Self>) -> anyhow::Result<()> {
        loop {
            let Some(connecting) = self.endpoint.accept().await else {
                break;
            };
            let accepted = Arc::clone(&self.accepted_tokens);
            let allowlist = Arc::clone(&self.allowlist);
            tokio::spawn(async move {
                if let Err(error) = handle_connection(connecting, accepted, allowlist).await {
                    debug!("alleycat connection ended: {error:#}");
                }
            });
        }
        Ok(())
    }

    /// Initiate a graceful shutdown of the QUIC endpoint. The accept loop
    /// in [`serve_shared`] / [`serve`] will return cleanly once the
    /// endpoint stops yielding incoming connections. Used by the daemon's
    /// `Stop` control handler.
    pub fn shutdown(&self) {
        self.endpoint.close(0u32.into(), b"shutting down");
    }
}

impl AsyncRead for RelayStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.project().recv.poll_read(cx, buf)
    }
}

impl AsyncWrite for RelayStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        self.project()
            .send
            .poll_write(cx, buf)
            .map_err(map_write_error)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        self.project().send.poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        self.project().send.poll_shutdown(cx)
    }
}

fn build_server_config(identity: &Identity) -> anyhow::Result<quinn::ServerConfig> {
    let server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![rustls::pki_types::CertificateDer::from(
                identity.cert_der.clone(),
            )],
            rustls::pki_types::PrivatePkcs8KeyDer::from(identity.key_der.clone()).into(),
        )?;
    Ok(quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)?,
    )))
}

async fn handle_connection(
    connecting: quinn::Incoming,
    accepted_tokens: Arc<ArcSwap<Vec<String>>>,
    allowlist: Arc<ArcSwap<Vec<Target>>>,
) -> anyhow::Result<()> {
    let connection = connecting.await?;
    let (mut control_send, mut control_recv) = connection.accept_bi().await?;
    let hello: ConnectHello = read_frame_json(&mut control_recv).await?;
    if hello.protocol_version != PROTOCOL_VERSION {
        write_frame_json(
            &mut control_send,
            &ConnectAck {
                protocol_version: PROTOCOL_VERSION,
                server_version: env!("CARGO_PKG_VERSION").to_string(),
                accepted: false,
                error: Some(format!(
                    "protocol mismatch: client={} relay={}",
                    hello.protocol_version, PROTOCOL_VERSION
                )),
            },
        )
        .await?;
        connection.close(0u32.into(), b"protocol mismatch");
        return Err(anyhow!("protocol mismatch"));
    }
    let token_ok = accepted_tokens.load().contains(&hello.token);
    if !token_ok {
        write_frame_json(
            &mut control_send,
            &ConnectAck {
                protocol_version: PROTOCOL_VERSION,
                server_version: env!("CARGO_PKG_VERSION").to_string(),
                accepted: false,
                error: Some("invalid token".to_string()),
            },
        )
        .await?;
        connection.close(0u32.into(), b"bad token");
        return Err(anyhow!("invalid token"));
    }
    write_frame_json(
        &mut control_send,
        &ConnectAck {
            protocol_version: PROTOCOL_VERSION,
            server_version: env!("CARGO_PKG_VERSION").to_string(),
            accepted: true,
            error: None,
        },
    )
    .await?;

    loop {
        let (send, recv) = match connection.accept_bi().await {
            Ok(streams) => streams,
            Err(quinn::ConnectionError::ApplicationClosed(_)) => break,
            Err(error) => return Err(anyhow!(error)),
        };
        let allowlist = Arc::clone(&allowlist);
        tokio::spawn(async move {
            if let Err(error) = handle_stream(send, recv, allowlist).await {
                debug!("alleycat stream failed: {error:#}");
            }
        });
    }
    Ok(())
}

async fn handle_stream(
    mut send: SendStream,
    mut recv: RecvStream,
    allowlist: Arc<ArcSwap<Vec<Target>>>,
) -> anyhow::Result<()> {
    let request: OpenRequest = read_frame_json(&mut recv).await?;
    if !allowlist.load().contains(&request.target) {
        write_frame_json(
            &mut send,
            &OpenResponse::rejected("target is not in the relay allowlist"),
        )
        .await?;
        return Err(anyhow!("target rejected"));
    }

    match request.target {
        Target::Tcp { host, port } => {
            let stream = TcpStream::connect((host.as_str(), port))
                .await
                .with_context(|| format!("tcp connect failed for {host}:{port}"))?;
            write_frame_json(&mut send, &OpenResponse::accepted()).await?;
            let mut relay_stream = RelayStream { send, recv };
            let mut stream = stream;
            let _ = tokio::io::copy_bidirectional(&mut relay_stream, &mut stream).await;
        }
        Target::Unix { path } => {
            let stream = connect_unix(&path).await?;
            write_frame_json(&mut send, &OpenResponse::accepted()).await?;
            let mut relay_stream = RelayStream { send, recv };
            let mut stream = stream;
            let _ = tokio::io::copy_bidirectional(&mut relay_stream, &mut stream).await;
        }
    }

    Ok(())
}

async fn connect_unix(path: &str) -> anyhow::Result<UnixStream> {
    #[cfg(unix)]
    {
        UnixStream::connect(path)
            .await
            .with_context(|| format!("unix connect failed for {path}"))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(anyhow!(
            "unix socket targets are not supported on this platform"
        ))
    }
}

fn resolve_bind_addr(bind: &str, port: u16) -> anyhow::Result<SocketAddr> {
    (bind, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow!("failed to resolve bind address {bind}:{port}"))
}

async fn write_ready_file(path: &Path, ready: &ReadyFile) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let data = serde_json::to_vec_pretty(ready)?;
    fs::write(path, data).await?;
    Ok(())
}

pub fn random_token() -> String {
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn map_write_error(error: quinn::WriteError) -> std::io::Error {
    std::io::Error::other(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bind_with_identity_uses_supplied_cert_and_token() {
        let identity = Identity::generate().expect("generate identity");
        let expected_fingerprint = hex::encode(Sha256::digest(&identity.cert_der));
        let expected_token = identity.token.clone();
        let runtime = RelayRuntime::bind_with_identity(
            RelayConfig {
                bind: "127.0.0.1".into(),
                udp_port: 0,
                ready_file: None,
                allowlist: Vec::new(),
            },
            identity,
        )
        .await
        .expect("bind");
        assert_eq!(runtime.ready().cert_fingerprint, expected_fingerprint);
        assert_eq!(runtime.ready().token, expected_token);
    }

    #[tokio::test]
    async fn swap_identity_accepts_old_and_new_during_drain() {
        let original = Identity::generate().expect("generate identity");
        let original_token = original.token.clone();
        let runtime = RelayRuntime::bind_with_identity(
            RelayConfig {
                bind: "127.0.0.1".into(),
                udp_port: 0,
                ready_file: None,
                allowlist: Vec::new(),
            },
            original,
        )
        .await
        .expect("bind");

        let next = Identity::generate().expect("generate replacement");
        let next_token = next.token.clone();
        runtime
            .swap_identity(next, Duration::from_secs(60))
            .await
            .expect("swap");

        let accepted = runtime.accepted_tokens.load();
        assert!(accepted.contains(&original_token));
        assert!(accepted.contains(&next_token));
        assert_eq!(runtime.ready().token, next_token);
    }

    #[tokio::test]
    async fn swap_identity_prunes_old_token_after_drain() {
        let original = Identity::generate().expect("generate identity");
        let original_token = original.token.clone();
        let runtime = RelayRuntime::bind_with_identity(
            RelayConfig {
                bind: "127.0.0.1".into(),
                udp_port: 0,
                ready_file: None,
                allowlist: Vec::new(),
            },
            original,
        )
        .await
        .expect("bind");

        let next = Identity::generate().expect("generate replacement");
        let next_token = next.token.clone();
        runtime
            .swap_identity(next, Duration::from_millis(50))
            .await
            .expect("swap");

        tokio::time::sleep(Duration::from_millis(250)).await;
        let accepted = runtime.accepted_tokens.load();
        assert!(!accepted.contains(&original_token));
        assert!(accepted.contains(&next_token));
    }

    #[tokio::test]
    async fn swap_identity_works_through_shared_arc() {
        // Mirrors the daemon usage: Arc<RelayRuntime> shared between an
        // accept-loop task and a control handler. We don't actually run
        // serve_shared here (no real client), but proving swap_identity
        // mutates state via &self through the Arc is enough.
        let runtime = Arc::new(
            RelayRuntime::bind_with_identity(
                RelayConfig {
                    bind: "127.0.0.1".into(),
                    udp_port: 0,
                    ready_file: None,
                    allowlist: Vec::new(),
                },
                Identity::generate().expect("identity"),
            )
            .await
            .expect("bind"),
        );
        let next = Identity::generate().expect("replacement");
        let expected_token = next.token.clone();
        let runtime_for_handler = Arc::clone(&runtime);
        runtime_for_handler
            .swap_identity(next, Duration::from_secs(60))
            .await
            .expect("swap");
        assert_eq!(runtime.ready().token, expected_token);
    }

    #[tokio::test]
    async fn set_allowlist_replaces_active_targets() {
        let runtime = RelayRuntime::bind(RelayConfig {
            bind: "127.0.0.1".into(),
            udp_port: 0,
            ready_file: None,
            allowlist: vec![Target::Tcp {
                host: "127.0.0.1".into(),
                port: 1,
            }],
        })
        .await
        .expect("bind");
        let next = vec![
            Target::Tcp {
                host: "127.0.0.1".into(),
                port: 2,
            },
            Target::Unix {
                path: "/tmp/x.sock".into(),
            },
        ];
        runtime.set_allowlist(next.clone());
        assert_eq!(runtime.allowlist(), next);
    }

    #[test]
    fn random_token_is_32_hex_chars() {
        let t = random_token();
        assert_eq!(t.len(), 32);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
