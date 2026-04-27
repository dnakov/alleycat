use std::collections::HashSet;
use std::sync::Mutex;

use tokio::sync::oneshot;

use crate::{RequestId, envelope::JsonRpcError};

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

#[derive(Default)]
pub struct ConnectionCoreState {
    capabilities: Mutex<Capabilities>,
    next_request_id: Mutex<i64>,
}

impl ConnectionCoreState {
    pub fn set_capabilities(&self, capabilities: Capabilities) {
        *self.capabilities.lock().unwrap() = capabilities;
    }

    pub fn capabilities(&self) -> Capabilities {
        self.capabilities.lock().unwrap().clone()
    }

    pub fn should_emit(&self, method: &str) -> bool {
        !self
            .capabilities
            .lock()
            .unwrap()
            .opt_out_notification_methods
            .contains(method)
    }

    pub fn next_request_id(&self) -> RequestId {
        let mut slot = self.next_request_id.lock().unwrap();
        *slot += 1;
        RequestId::String(format!("bridge-{}", *slot))
    }
}
