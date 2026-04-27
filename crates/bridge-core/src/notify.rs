use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, mpsc, oneshot};

use crate::envelope::{
    JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, JsonRpcVersion, RequestId,
};
use crate::state::{ConnectionCoreState, PendingServerRequest, ServerRequestError};

#[derive(Clone)]
pub struct NotificationSender {
    tx: mpsc::UnboundedSender<JsonRpcMessage>,
    core: Arc<ConnectionCoreState>,
    pending: Arc<Mutex<HashMap<RequestId, PendingServerRequest>>>,
}

impl NotificationSender {
    pub fn new(
        tx: mpsc::UnboundedSender<JsonRpcMessage>,
        core: Arc<ConnectionCoreState>,
        pending: Arc<Mutex<HashMap<RequestId, PendingServerRequest>>>,
    ) -> Self {
        Self { tx, core, pending }
    }

    pub fn send_notification(
        &self,
        method: impl Into<String>,
        params: impl serde::Serialize,
    ) -> anyhow::Result<()> {
        let method = method.into();
        if !self.core.should_emit(&method) {
            return Ok(());
        }
        self.tx
            .send(JsonRpcMessage::Notification(JsonRpcNotification {
                jsonrpc: JsonRpcVersion,
                method,
                params: Some(serde_json::to_value(params)?),
            }))?;
        Ok(())
    }

    pub fn send_message(&self, message: JsonRpcMessage) -> anyhow::Result<()> {
        self.tx.send(message)?;
        Ok(())
    }

    pub async fn request(
        &self,
        method: impl Into<String>,
        params: impl serde::Serialize,
        timeout: Duration,
    ) -> Result<serde_json::Value, ServerRequestError> {
        let method = method.into();
        let id = self.core.next_request_id();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(
            id.clone(),
            PendingServerRequest {
                method: method.clone(),
                responder: tx,
            },
        );
        if self
            .tx
            .send(JsonRpcMessage::Request(JsonRpcRequest {
                jsonrpc: JsonRpcVersion,
                id: id.clone(),
                method,
                params: Some(serde_json::to_value(params).map_err(|err| {
                    ServerRequestError::Rpc(crate::JsonRpcError::internal(err.to_string()))
                })?),
            }))
            .is_err()
        {
            self.pending.lock().await.remove(&id);
            return Err(ServerRequestError::ConnectionClosed);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(ServerRequestError::ConnectionClosed),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(ServerRequestError::TimedOut)
            }
        }
    }

    pub async fn resolve_response(&self, response: JsonRpcResponse) -> bool {
        let Some(pending) = self.pending.lock().await.remove(&response.id) else {
            return false;
        };
        let result = match response.error {
            Some(error) => Err(ServerRequestError::Rpc(error)),
            None => Ok(response.result.unwrap_or(serde_json::Value::Null)),
        };
        let _ = pending.responder.send(result);
        true
    }

    pub async fn cancel_all(&self) {
        for (_, pending) in self.pending.lock().await.drain() {
            let _ = pending
                .responder
                .send(Err(ServerRequestError::ConnectionClosed));
        }
    }
}
