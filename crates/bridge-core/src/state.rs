use std::collections::HashSet;

use tokio::sync::oneshot;

use crate::envelope::JsonRpcError;

#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    pub experimental_api: bool,
    pub opt_out_notification_methods: HashSet<String>,
    pub client_name: Option<String>,
    pub client_title: Option<String>,
    pub client_version: Option<String>,
}

#[derive(Debug)]
pub enum ServerRequestError {
    Rpc(JsonRpcError),
    ConnectionClosed,
    TimedOut,
}

pub struct PendingServerRequest {
    pub method: String,
    pub responder: oneshot::Sender<Result<serde_json::Value, ServerRequestError>>,
}
