use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use alleycat_protocol::{
    ConnectAck, ConnectHello, OpenRequest, OpenResponse, PROTOCOL_VERSION, ReadyFile, Target,
    read_frame_json, write_frame_json,
};
use anyhow::{Context, anyhow};
use pin_project_lite::pin_project;
use quinn::{Endpoint, RecvStream, SendStream};
use rand::RngCore;
use rcgen::generate_simple_self_signed;
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpStream, UnixStream};
use tracing::debug;

#[derive(Debug, Clone)]
pub struct RelayConfig {
    pub bind: String,
    pub udp_port: u16,
    pub ready_file: PathBuf,
    pub allowlist: Vec<Target>,
}

pub struct RelayRuntime {
    endpoint: Endpoint,
    ready: ReadyFile,
    allowlist: Arc<Vec<Target>>,
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
    pub async fn bind(config: RelayConfig) -> anyhow::Result<Self> {
        let cert = generate_simple_self_signed(vec!["alleycat.invalid".to_string()])?;
        let cert_der = cert.cert.der().to_vec();
        let key_der = cert.key_pair.serialize_der();
        let fingerprint = hex::encode(Sha256::digest(&cert_der));

        let server_crypto = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![rustls::pki_types::CertificateDer::from(cert_der.clone())],
                rustls::pki_types::PrivatePkcs8KeyDer::from(key_der).into(),
            )?;
        let server_config = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)?,
        ));

        let addr = resolve_bind_addr(&config.bind, config.udp_port)?;
        let endpoint = Endpoint::server(server_config, addr)?;
        let local_port = endpoint.local_addr()?.port();
        let token = random_token();
        let ready = ReadyFile {
            protocol_version: PROTOCOL_VERSION,
            udp_port: local_port,
            cert_fingerprint: fingerprint,
            token,
            pid: std::process::id(),
            allowlist: config.allowlist.clone(),
        };
        write_ready_file(&config.ready_file, &ready).await?;
        Ok(Self {
            endpoint,
            ready,
            allowlist: Arc::new(config.allowlist),
        })
    }

    pub fn ready(&self) -> &ReadyFile {
        &self.ready
    }

    pub async fn serve(self) -> anyhow::Result<()> {
        loop {
            let Some(connecting) = self.endpoint.accept().await else {
                break;
            };
            let token = self.ready.token.clone();
            let allowlist = Arc::clone(&self.allowlist);
            tokio::spawn(async move {
                if let Err(error) = handle_connection(connecting, token, allowlist).await {
                    debug!("alleycat connection ended: {error:#}");
                }
            });
        }
        Ok(())
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

async fn handle_connection(
    connecting: quinn::Incoming,
    expected_token: String,
    allowlist: Arc<Vec<Target>>,
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
    if hello.token != expected_token {
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
    allowlist: Arc<Vec<Target>>,
) -> anyhow::Result<()> {
    let request: OpenRequest = read_frame_json(&mut recv).await?;
    if !allowlist.contains(&request.target) {
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
            let mut relay_stream = RelayStream {
                send,
                recv,
            };
            let mut stream = stream;
            let _ = tokio::io::copy_bidirectional(&mut relay_stream, &mut stream).await;
        }
        Target::Unix { path } => {
            let stream = connect_unix(&path).await?;
            write_frame_json(&mut send, &OpenResponse::accepted()).await?;
            let mut relay_stream = RelayStream {
                send,
                recv,
            };
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
        Err(anyhow!("unix socket targets are not supported on this platform"))
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

fn random_token() -> String {
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn map_write_error(error: quinn::WriteError) -> std::io::Error {
    std::io::Error::other(error)
}
