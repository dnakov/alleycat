use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, warn};

use crate::RequestId;
use crate::envelope::{
    InboundMessage, JsonRpcError, JsonRpcMessage, JsonRpcResponse, JsonRpcVersion, error_codes,
};
use crate::framing::{read_json_line, write_json_line};
use crate::notify::NotificationSender;
use crate::state::{Capabilities, ConnectionCoreState, PendingServerRequest};

#[derive(Clone)]
pub struct Conn {
    core: Arc<ConnectionCoreState>,
    notifier: NotificationSender,
}

impl Conn {
    pub fn notifier(&self) -> &NotificationSender {
        &self.notifier
    }

    pub fn capabilities(&self) -> Capabilities {
        self.core.capabilities()
    }

    pub fn should_emit(&self, method: &str) -> bool {
        self.core.should_emit(method)
    }

    pub fn set_initialize_capabilities(&self, params: &Value) {
        let client_info = params.get("clientInfo");
        let capabilities = params.get("capabilities");
        let opt_out = capabilities
            .and_then(|value| value.get("optOutNotificationMethods"))
            .and_then(|value| value.as_array())
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(ToOwned::to_owned))
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default();
        self.core.set_capabilities(Capabilities {
            experimental_api: capabilities
                .and_then(|value| value.get("experimentalApi"))
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
            opt_out_notification_methods: opt_out,
            client_name: client_info
                .and_then(|value| value.get("name"))
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned),
            client_title: client_info
                .and_then(|value| value.get("title"))
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned),
            client_version: client_info
                .and_then(|value| value.get("version"))
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned),
        });
    }
}

#[async_trait]
pub trait Bridge: Send + Sync + 'static {
    async fn initialize(&self, ctx: &Conn, params: Value) -> Result<Value, JsonRpcError>;
    async fn dispatch(
        &self,
        ctx: &Conn,
        method: &str,
        params: Value,
    ) -> Result<Value, JsonRpcError>;

    async fn notification(&self, _ctx: &Conn, _method: &str, _params: Value) {}
}

#[derive(Debug, Clone)]
pub struct ServerOptions {
    pub socket_path: PathBuf,
    pub unlink_stale: bool,
}

pub async fn serve_unix<B>(bridge: Arc<B>, options: ServerOptions) -> anyhow::Result<()>
where
    B: Bridge,
{
    bind_unix_socket(&options.socket_path, options.unlink_stale)?;
    let listener = UnixListener::bind(&options.socket_path)?;
    loop {
        let (stream, _) = listener.accept().await?;
        let bridge = Arc::clone(&bridge);
        tokio::spawn(async move {
            if let Err(error) = serve_stream(bridge, stream).await {
                debug!("bridge connection ended: {error:#}");
            }
        });
    }
}

fn bind_unix_socket(path: &Path, unlink_stale: bool) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if unlink_stale {
        match std::fs::remove_file(path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub async fn serve_stream<B, S>(bridge: Arc<B>, stream: S) -> anyhow::Result<()>
where
    B: Bridge,
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();
    let core = Arc::new(ConnectionCoreState::default());
    let pending = Arc::new(Mutex::new(HashMap::<RequestId, PendingServerRequest>::new()));
    let notifier = NotificationSender::new(out_tx, Arc::clone(&core), Arc::clone(&pending));
    let conn = Conn { core, notifier };

    let writer_task = tokio::spawn(async move {
        while let Some(message) = out_rx.recv().await {
            if let Err(error) = write_json_line(&mut writer, &message).await {
                warn!("bridge writer failed: {error:#}");
                break;
            }
        }
    });

    while let Some(value) = read_json_line::<Value, _>(&mut reader).await? {
        let inbound = match InboundMessage::from_value(value) {
            Ok(inbound) => inbound,
            Err(error) => {
                warn!("discarding malformed json-rpc frame: {error}");
                continue;
            }
        };
        match inbound {
            InboundMessage::Request(request) => {
                let bridge = Arc::clone(&bridge);
                let conn = conn.clone();
                tokio::spawn(async move {
                    let id = request.id;
                    let method = request.method;
                    let params = request.params.unwrap_or(Value::Null);
                    let result = if method == "initialize" {
                        conn.set_initialize_capabilities(&params);
                        bridge.initialize(&conn, params).await
                    } else {
                        bridge.dispatch(&conn, &method, params).await
                    };
                    let response = match result {
                        Ok(result) => JsonRpcResponse {
                            jsonrpc: JsonRpcVersion,
                            id,
                            result: Some(result),
                            error: None,
                        },
                        Err(error) => JsonRpcResponse {
                            jsonrpc: JsonRpcVersion,
                            id,
                            result: None,
                            error: Some(error),
                        },
                    };
                    let _ = conn
                        .notifier
                        .send_message(JsonRpcMessage::Response(response));
                });
            }
            InboundMessage::Notification(notification) => {
                bridge
                    .notification(
                        &conn,
                        &notification.method,
                        notification.params.unwrap_or(Value::Null),
                    )
                    .await;
            }
            InboundMessage::Response(response) => {
                conn.notifier.resolve_response(response).await;
            }
        }
    }
    conn.notifier.cancel_all().await;
    drop(conn);
    let _ = writer_task.await;
    Ok(())
}

pub fn json_error_from_anyhow(error: anyhow::Error) -> JsonRpcError {
    JsonRpcError {
        code: error_codes::INTERNAL_ERROR,
        message: format!("{error:#}"),
        data: None,
    }
}
