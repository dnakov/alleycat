use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Target {
    Tcp { host: String, port: u16 },
    Unix { path: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectHello {
    pub protocol_version: u32,
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectAck {
    pub protocol_version: u32,
    pub server_version: String,
    pub accepted: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenRequest {
    pub target: Target,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenResponse {
    pub accepted: bool,
    pub error: Option<String>,
}

impl OpenResponse {
    pub fn accepted() -> Self {
        Self {
            accepted: true,
            error: None,
        }
    }

    pub fn rejected(message: impl Into<String>) -> Self {
        Self {
            accepted: false,
            error: Some(message.into()),
        }
    }
}
