//! JSON-RPC 2.0 envelopes (request/response/notification/error) used by the
//! codex app-server protocol. Generic enough that the bridge dispatch loop can
//! parse the method off and dispatch by string without first deserializing the
//! params.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

/// Request/notification id. Codex emits both numeric and string ids.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(untagged)]
pub enum RequestId {
    Integer(i64),
    String(String),
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestId::Integer(n) => write!(f, "{n}"),
            RequestId::String(s) => write!(f, "{s}"),
        }
    }
}

/// Inbound JSON-RPC request from the codex client.
///
/// `jsonrpc` is `#[serde(default)]` because some real-world codex clients
/// (notably `codex-app-server-test-client`) omit the version field even though
/// the spec says it should be `"2.0"`. Defaulting to `JsonRpcVersion`
/// transparently fills it in; explicit non-`"2.0"` values still error via the
/// custom `Deserialize` impl below.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct JsonRpcRequest {
    #[serde(default)]
    pub jsonrpc: JsonRpcVersion,
    pub id: RequestId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// Inbound JSON-RPC notification (no `id`).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct JsonRpcNotification {
    #[serde(default)]
    pub jsonrpc: JsonRpcVersion,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// Inbound JSON-RPC response to a server-initiated request (e.g. approval
/// callbacks the bridge sent the client).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct JsonRpcResponse {
    #[serde(default)]
    pub jsonrpc: JsonRpcVersion,
    pub id: RequestId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// Outbound JSON-RPC envelope: a request, response, notification, or error
/// reply. `untagged` lets us reuse the same line-oriented framing that codex
/// itself uses on stdio.
#[derive(Serialize, Debug, Clone)]
#[serde(untagged)]
pub enum JsonRpcMessage {
    Request(JsonRpcRequest),
    Response(JsonRpcResponse),
    Notification(JsonRpcNotification),
}

/// JSON-RPC protocol version literal `"2.0"`. Wrapped in a newtype so any
/// non-`"2.0"` payload deserializes as an error (catches malformed clients
/// early), and so the literal serializes verbatim on outbound frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonRpcVersion;

impl JsonRpcVersion {
    pub const LITERAL: &'static str = "2.0";
}

impl Default for JsonRpcVersion {
    fn default() -> Self {
        Self
    }
}

impl Serialize for JsonRpcVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(Self::LITERAL)
    }
}

impl<'de> Deserialize<'de> for JsonRpcVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        if s == Self::LITERAL {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom(format!(
                "expected jsonrpc version \"2.0\", got {s:?}"
            )))
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Standard JSON-RPC 2.0 error codes plus a few app-server-specific ones the
/// codex test client cares about.
pub mod error_codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
}

/// Unified inbound message off the wire — one of three shapes. Codex clients
/// can send a request, a notification, or a response (when replying to a
/// server-initiated request).
#[derive(Debug, Clone)]
pub enum InboundMessage {
    Request(JsonRpcRequest),
    Notification(JsonRpcNotification),
    Response(JsonRpcResponse),
}

impl InboundMessage {
    /// Parse a single JSON-RPC frame. Distinguishes request vs notification by
    /// presence of `id`, and request vs response by presence of `method`.
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        let has_id = value.get("id").is_some();
        let has_method = value.get("method").is_some();
        match (has_id, has_method) {
            (true, true) => Ok(InboundMessage::Request(serde_json::from_value(value)?)),
            (true, false) => Ok(InboundMessage::Response(serde_json::from_value(value)?)),
            (false, true) => Ok(InboundMessage::Notification(serde_json::from_value(value)?)),
            (false, false) => Err(serde::de::Error::custom(
                "json-rpc frame missing both `id` and `method`",
            )),
        }
    }
}
