use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
pub const ALLEYCAT_ALPN: &[u8] = b"alleycat/1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairPayload {
    pub v: u32,
    pub node_id: String,
    pub token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentWire {
    Websocket,
    Jsonl,
}

impl AgentWire {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Websocket => "websocket",
            Self::Jsonl => "jsonl",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentInfo {
    pub name: String,
    pub display_name: String,
    pub wire: AgentWire,
    pub available: bool,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    ListAgents {
        v: u32,
        token: String,
    },
    Connect {
        v: u32,
        token: String,
        agent: String,
    },
}

impl Request {
    pub fn version(&self) -> u32 {
        match self {
            Self::ListAgents { v, .. } | Self::Connect { v, .. } => *v,
        }
    }

    pub fn token(&self) -> &str {
        match self {
            Self::ListAgents { token, .. } | Self::Connect { token, .. } => token,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub v: u32,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<AgentInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok() -> Self {
        Self {
            v: PROTOCOL_VERSION,
            ok: true,
            agents: None,
            error: None,
        }
    }

    pub fn agents(agents: Vec<AgentInfo>) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            ok: true,
            agents: Some(agents),
            error: None,
        }
    }

    pub fn error(error: impl Into<String>) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            ok: false,
            agents: None,
            error: Some(error.into()),
        }
    }
}
