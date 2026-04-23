use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use alleycat_protocol::{
    ConnectAck, ConnectHello, FrameError, OpenRequest, OpenResponse, PROTOCOL_VERSION,
    read_frame_json, write_frame_json,
};
use pin_project_lite::pin_project;
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, lookup_host};
use tokio::sync::Mutex;
use tracing::{debug, warn};

pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub use alleycat_protocol::{PROTOCOL_VERSION as WIRE_PROTOCOL_VERSION, ReadyFile, Target};

#[derive(Debug, Clone)]
pub struct ConnectParams {
    pub host: String,
    pub port: u16,
    pub cert_fingerprint: String,
    pub token: String,
    pub protocol_version: u32,
}

#[derive(Debug, Clone)]
pub struct ForwardSpec {
    pub local_port: u16,
    pub target: Target,
}

#[derive(Clone, Debug)]
pub struct Session {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    state: Mutex<SessionState>,
    shutdown: AtomicBool,
}

#[derive(Debug)]
struct SessionState {
    endpoint: Endpoint,
    params: ConnectParams,
    connection: Option<Connection>,
    forward_tasks: HashMap<u16, tokio::task::JoinHandle<()>>,
}

#[derive(Clone, Debug)]
pub struct ForwardHandle {
    session: Session,
    local_port: u16,
}

pin_project! {
    #[derive(Debug)]
    pub struct AlleycatStream {
        #[pin]
        send: SendStream,
        #[pin]
        recv: RecvStream,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("transport error: {0}")]
    Transport(#[from] anyhow::Error),
    #[error("frame error: {0}")]
    Frame(#[from] FrameError),
    #[error("relay rejected connect: {0}")]
    RelayRejected(String),
    #[error("session is shutting down")]
    Shutdown,
}

impl Session {
    pub async fn connect(params: ConnectParams) -> Result<Self, SessionError> {
        let bind_addr: SocketAddr = "[::]:0".parse().expect("valid client bind addr");
        let mut endpoint = Endpoint::client(bind_addr).map_err(anyhow::Error::new)?;
        endpoint.set_default_client_config(build_client_config(&params)?);
        let session = Self {
            inner: Arc::new(Inner {
                state: Mutex::new(SessionState {
                    endpoint,
                    params,
                    connection: None,
                    forward_tasks: HashMap::new(),
                }),
                shutdown: AtomicBool::new(false),
            }),
        };
        session.reconnect().await?;
        Ok(session)
    }

    pub async fn reconnect(&self) -> Result<(), SessionError> {
        let mut state = self.inner.state.lock().await;
        if self.inner.shutdown.load(Ordering::SeqCst) {
            return Err(SessionError::Shutdown);
        }

        if let Some(existing) = state.connection.take() {
            existing.close(0u32.into(), b"reconnect");
        }

        let connection = connect_once(&state.endpoint, &state.params).await?;
        state.connection = Some(connection);
        Ok(())
    }

    pub async fn update_connect_params(&self, params: ConnectParams) -> Result<(), SessionError> {
        let mut state = self.inner.state.lock().await;
        let bind_addr: SocketAddr = "[::]:0".parse().expect("valid client bind addr");
        let mut endpoint = Endpoint::client(bind_addr).map_err(anyhow::Error::new)?;
        endpoint.set_default_client_config(build_client_config(&params)?);
        state.params = params.clone();
        if let Some(existing) = state.connection.take() {
            existing.close(0u32.into(), b"reconfigure");
        }
        state.endpoint.close(0u32.into(), b"reconfigure");
        state.endpoint = endpoint;
        let connection = connect_once(&state.endpoint, &params).await?;
        state.connection = Some(connection);
        Ok(())
    }

    pub async fn open_stream(&self, target: Target) -> Result<AlleycatStream, SessionError> {
        let request = OpenRequest { target };
        let mut last_error: Option<anyhow::Error> = None;
        for _attempt in 0..2 {
            let connection = self.ensure_connection().await?;
            match connection.open_bi().await {
                Ok((mut send, mut recv)) => {
                    write_frame_json(&mut send, &request).await?;
                    let response: OpenResponse = read_frame_json(&mut recv).await?;
                    if !response.accepted {
                        return Err(SessionError::RelayRejected(
                            response
                                .error
                                .unwrap_or_else(|| "target rejected".to_string()),
                        ));
                    }
                    return Ok(AlleycatStream { send, recv });
                }
                Err(error) => {
                    last_error = Some(anyhow::Error::new(error));
                    self.reconnect().await?;
                }
            }
        }
        Err(SessionError::Transport(
            last_error.unwrap_or_else(|| anyhow::anyhow!("failed to open relay stream")),
        ))
    }

    pub async fn ensure_forward(&self, spec: ForwardSpec) -> Result<ForwardHandle, SessionError> {
        let listener = TcpListener::bind(("127.0.0.1", spec.local_port))
            .await
            .map_err(anyhow::Error::new)?;
        let actual_port = listener.local_addr().map_err(anyhow::Error::new)?.port();
        let session = self.clone();
        let target = spec.target.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut local_stream, peer_addr) = match listener.accept().await {
                    Ok(value) => value,
                    Err(error) => {
                        warn!("alleycat forward accept error on {actual_port}: {error}");
                        break;
                    }
                };
                let session = session.clone();
                let target = target.clone();
                tokio::spawn(async move {
                    match session.open_stream(target).await {
                        Ok(mut remote_stream) => {
                            if let Err(error) =
                                tokio::io::copy_bidirectional(&mut local_stream, &mut remote_stream)
                                    .await
                            {
                                debug!(
                                    "alleycat forward proxy error port={} peer={} error={}",
                                    actual_port, peer_addr, error
                                );
                            }
                            let _ = remote_stream.shutdown().await;
                        }
                        Err(error) => {
                            warn!(
                                "alleycat forward open_stream failed port={} peer={} error={}",
                                actual_port, peer_addr, error
                            );
                        }
                    }
                });
            }
        });
        let mut state = self.inner.state.lock().await;
        if let Some(existing) = state.forward_tasks.insert(actual_port, task) {
            existing.abort();
        }
        Ok(ForwardHandle {
            session: self.clone(),
            local_port: actual_port,
        })
    }

    pub async fn abort_forward(&self, local_port: u16) -> bool {
        let mut state = self.inner.state.lock().await;
        if let Some(task) = state.forward_tasks.remove(&local_port) {
            task.abort();
            true
        } else {
            false
        }
    }

    pub async fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
        let mut state = self.inner.state.lock().await;
        for (_, task) in state.forward_tasks.drain() {
            task.abort();
        }
        if let Some(connection) = state.connection.take() {
            connection.close(0u32.into(), b"shutdown");
        }
        state.endpoint.close(0u32.into(), b"shutdown");
    }

    async fn ensure_connection(&self) -> Result<Connection, SessionError> {
        let mut state = self.inner.state.lock().await;
        if self.inner.shutdown.load(Ordering::SeqCst) {
            return Err(SessionError::Shutdown);
        }
        let must_reconnect = match state.connection.as_ref() {
            Some(connection) => connection.close_reason().is_some(),
            None => true,
        };
        if must_reconnect {
            let params = state.params.clone();
            state.connection = Some(connect_once(&state.endpoint, &params).await?);
        }
        Ok(state
            .connection
            .as_ref()
            .expect("connection set after reconnect")
            .clone())
    }
}

impl ForwardHandle {
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    pub async fn close(&self) -> bool {
        self.session.abort_forward(self.local_port).await
    }
}

impl AsyncRead for AlleycatStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.project().recv.poll_read(cx, buf)
    }
}

impl AsyncWrite for AlleycatStream {
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

impl AlleycatStream {
    pub async fn shutdown(&mut self) -> std::io::Result<()> {
        AsyncWriteExt::shutdown(self).await
    }
}

fn build_client_config(params: &ConnectParams) -> Result<quinn::ClientConfig, SessionError> {
    let verifier = Arc::new(PinnedServerCertVerifier::new(
        params.cert_fingerprint.clone(),
    ));
    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
            .map_err(anyhow::Error::new)?,
    )))
}

async fn connect_once(endpoint: &Endpoint, params: &ConnectParams) -> Result<Connection, SessionError> {
    let addr = lookup_host((params.host.as_str(), params.port))
        .await
        .map_err(anyhow::Error::new)?
        .next()
        .ok_or_else(|| anyhow::anyhow!("failed to resolve {}:{}", params.host, params.port))?;
    let connect = endpoint
        .connect(addr, "alleycat.invalid")
        .map_err(anyhow::Error::new)?;
    let connection = connect.await.map_err(anyhow::Error::new)?;
    let (mut send, mut recv) = connection.open_bi().await.map_err(anyhow::Error::new)?;
    write_frame_json(
        &mut send,
        &ConnectHello {
            protocol_version: params.protocol_version,
            token: params.token.clone(),
        },
    )
    .await?;
    let ack: ConnectAck = read_frame_json(&mut recv).await?;
    if !ack.accepted {
        return Err(SessionError::RelayRejected(
            ack.error
                .unwrap_or_else(|| "relay rejected handshake".to_string()),
        ));
    }
    if ack.protocol_version != PROTOCOL_VERSION {
        return Err(SessionError::RelayRejected(format!(
            "protocol mismatch: client={} relay={}",
            PROTOCOL_VERSION, ack.protocol_version
        )));
    }
    Ok(connection)
}

#[derive(Debug)]
struct PinnedServerCertVerifier {
    expected_fingerprint: String,
}

impl PinnedServerCertVerifier {
    fn new(expected_fingerprint: String) -> Self {
        Self {
            expected_fingerprint: normalize_fingerprint(&expected_fingerprint),
        }
    }
}

impl ServerCertVerifier for PinnedServerCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let actual = certificate_fingerprint(end_entity.as_ref());
        if actual == self.expected_fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "pinned certificate mismatch expected={} actual={}",
                self.expected_fingerprint, actual
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn certificate_fingerprint(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn normalize_fingerprint(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn map_write_error(error: quinn::WriteError) -> std::io::Error {
    std::io::Error::other(error)
}
