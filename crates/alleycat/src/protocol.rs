use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
pub const ALLEYCAT_ALPN: &[u8] = b"alleycat/1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairPayload {
    pub v: u32,
    pub node_id: String,
    pub token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_name: Option<String>,
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

/// Resume hint sent on `Connect` when a reconnecting client wants to
/// reattach to an existing session for `(client_node_id, agent)`. The server
/// keys on the iroh `remote_node_id`, so the client doesn't need to (and
/// can't) carry its own identity here. `last_seq` is the highest seq the
/// client successfully observed on the prior attachment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Resume {
    pub last_seq: u64,
}

/// What `Connect` resolved to on the server side.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttachKind {
    /// No prior session existed; one was minted for this client.
    Fresh,
    /// Prior session reattached and the resume cursor was within the replay
    /// window. The drainer will replay missed frames before going live.
    Resumed,
    /// Prior session existed but the cursor predates the ring floor. The
    /// client must reload state from authoritative storage (e.g. `thread/read`)
    /// before treating this stream as caught up; backlog replay is empty.
    DriftReload,
}

impl From<alleycat_bridge_core::session::AttachKind> for AttachKind {
    fn from(value: alleycat_bridge_core::session::AttachKind) -> Self {
        match value {
            alleycat_bridge_core::session::AttachKind::Fresh => Self::Fresh,
            alleycat_bridge_core::session::AttachKind::Resumed => Self::Resumed,
            alleycat_bridge_core::session::AttachKind::DriftReload => Self::DriftReload,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionInfo {
    pub attached: AttachKind,
    pub current_seq: u64,
    pub floor_seq: u64,
}

#[derive(Debug, Serialize, Deserialize)]
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume: Option<Resume>,
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

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub v: u32,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<AgentInfo>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok_with_session(session: SessionInfo) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            ok: true,
            agents: None,
            session: Some(session),
            error: None,
        }
    }

    pub fn agents(agents: Vec<AgentInfo>) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            ok: true,
            agents: Some(agents),
            session: None,
            error: None,
        }
    }

    pub fn error(error: impl Into<String>) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            ok: false,
            agents: None,
            session: None,
            error: Some(error.into()),
        }
    }
}
