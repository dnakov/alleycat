//! HITL approval bridging for `--permission-prompt-tool stdio`.
//!
//! When the pool's `bypass_permissions` flag is OFF, claude is spawned with
//! `--permission-prompt-tool stdio` and emits an inbound
//! `control_request{subtype:"can_use_tool", tool_name, input, ...}` over
//! stdout for every tool call. The bridge translates that into a codex
//! `item/{commandExecution,fileChange}/requestApproval` server→client request,
//! awaits the connected client's `decision`, and replies via outbound
//! `control_response{response:{request_id, subtype:"success", response:{behavior:"allow"|"deny", ...}}}`.
//!
//! Wire-quirk note: both control_response directions nest `request_id`
//! INSIDE the outer `response` object — the SDK reads `Q.response.request_id`.
//! See `pool::claude_protocol`.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

use alleycat_codex_proto::{
    CommandExecutionRequestApprovalParams, FileChangeRequestApprovalParams, JsonRpcMessage,
    JsonRpcRequest, JsonRpcVersion, RequestId, ToolRequestUserInputAnswer,
    ToolRequestUserInputParams, ToolRequestUserInputQuestion, ToolRequestUserInputResponse,
};

use crate::pool::ClaudeProcessHandle;
use crate::pool::claude_protocol::{
    ClaudeInbound, OutboundControlResponseEnvelope, OutboundControlResponseInner,
};
use crate::state::{ConnectionState, ServerRequestError};
use crate::translate::tool_call::{CodexToolKind, classify};

/// Time the bridge waits for the codex client's decision before treating the
/// silence as a deny + interrupting claude. 5 minutes mirrors pi-bridge's
/// `DEFAULT_APPROVAL_TIMEOUT`.
pub const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Errors a HITL bridge can surface.
#[derive(Debug, Error)]
pub enum ApprovalError {
    #[error("connection to codex client closed before approval landed")]
    ConnectionClosed,
    #[error("approval request timed out after {0:?}")]
    Timeout(Duration),
    #[error("codex client returned error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("malformed approval response: {0}")]
    Malformed(String),
}

impl From<ServerRequestError> for ApprovalError {
    fn from(value: ServerRequestError) -> Self {
        match value {
            ServerRequestError::Rpc { code, message } => Self::Rpc { code, message },
            ServerRequestError::ConnectionClosed => Self::ConnectionClosed,
            ServerRequestError::TimedOut => Self::Timeout(DEFAULT_APPROVAL_TIMEOUT),
        }
    }
}

/// Parsed inbound `control_request{can_use_tool}`. Only the fields the bridge
/// reads are surfaced; everything else stays in the raw envelope and gets
/// dropped on the floor.
#[derive(Debug, Clone)]
pub struct CanUseToolRequest {
    pub request_id: String,
    pub tool_name: String,
    pub input: Value,
    pub tool_use_id: Option<String>,
    pub blocked_path: Option<String>,
    pub decision_reason: Option<String>,
}

/// Inspect a `ClaudeOutbound::ControlRequest(Value)` payload and extract a
/// typed [`CanUseToolRequest`] when the subtype matches. Returns `None` for
/// other inbound subtypes (`hook_callback`, `mcp_message`, ...) which the
/// bridge silently drops in v2-iter1.
pub fn parse_can_use_tool(value: &Value) -> Option<CanUseToolRequest> {
    let request_id = value.get("request_id")?.as_str()?.to_string();
    let request = value.get("request")?;
    let subtype = request.get("subtype")?.as_str()?;
    if subtype != "can_use_tool" {
        return None;
    }
    let tool_name = request.get("tool_name")?.as_str()?.to_string();
    let input = request.get("input").cloned().unwrap_or(Value::Null);
    Some(CanUseToolRequest {
        request_id,
        tool_name,
        input,
        tool_use_id: request
            .get("tool_use_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        blocked_path: request
            .get("blocked_path")
            .and_then(Value::as_str)
            .map(str::to_string),
        decision_reason: request
            .get("decision_reason")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Bucketed view of the codex client's decision. Mirrors pi-bridge's
/// `ApprovalOutcome` so future shared-helper extraction is a noop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOutcome {
    Approved,
    Declined,
    Cancelled,
}

fn bucket_decision(value: &Value) -> ApprovalOutcome {
    let tag = match value {
        Value::String(s) => s.as_str(),
        Value::Object(m) => match m.keys().next() {
            Some(k) => k.as_str(),
            None => return ApprovalOutcome::Declined,
        },
        _ => return ApprovalOutcome::Declined,
    };
    match tag {
        "accept"
        | "acceptForSession"
        | "acceptWithExecpolicyAmendment"
        | "applyNetworkPolicyAmendment" => ApprovalOutcome::Approved,
        "decline" => ApprovalOutcome::Declined,
        "cancel" => ApprovalOutcome::Cancelled,
        other => {
            tracing::warn!(
                decision = %other,
                "unknown codex approval decision tag — treating as decline"
            );
            ApprovalOutcome::Declined
        }
    }
}

/// Resolve a `can_use_tool` inbound control_request: classify the tool,
/// surface a codex `requestApproval` request, await the client's decision,
/// and reply on the claude side with the matching `control_response`.
///
/// On any failure (connection closed, timeout, malformed reply) the caller
/// gets `Err` and the function will NOT have sent a `control_response` —
/// callers should treat that as a hard deny and either retry or kill the
/// process. The pump in `handlers/turn.rs` chooses to deny + interrupt.
pub async fn handle_can_use_tool(
    state: &Arc<ConnectionState>,
    handle: &Arc<ClaudeProcessHandle>,
    thread_id: &str,
    turn_id: &str,
    req: CanUseToolRequest,
) -> Result<ApprovalOutcome, ApprovalError> {
    let kind = classify(&req.tool_name);
    let item_id = req
        .tool_use_id
        .clone()
        .unwrap_or_else(|| Uuid::now_v7().to_string());

    let outcome =
        match kind {
            CodexToolKind::CommandExecution => {
                let command = req
                    .input
                    .get("command")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let params = CommandExecutionRequestApprovalParams {
                    thread_id: thread_id.to_string(),
                    turn_id: turn_id.to_string(),
                    item_id: item_id.clone(),
                    command,
                    reason: req.decision_reason.clone(),
                    ..Default::default()
                };
                let value = send_server_request(
                    state,
                    "item/commandExecution/requestApproval",
                    serde_json::to_value(&params)
                        .map_err(|e| ApprovalError::Malformed(format!("encode params: {e}")))?,
                    None,
                )
                .await?;
                bucket_decision(value.get("decision").ok_or_else(|| {
                    ApprovalError::Malformed("response missing `decision`".into())
                })?)
            }
            CodexToolKind::FileChange => {
                let params = FileChangeRequestApprovalParams {
                    thread_id: thread_id.to_string(),
                    turn_id: turn_id.to_string(),
                    item_id: item_id.clone(),
                    reason: req.decision_reason.clone(),
                    grant_root: req.blocked_path.clone(),
                };
                let value = send_server_request(
                    state,
                    "item/fileChange/requestApproval",
                    serde_json::to_value(&params)
                        .map_err(|e| ApprovalError::Malformed(format!("encode params: {e}")))?,
                    None,
                )
                .await?;
                bucket_decision(value.get("decision").ok_or_else(|| {
                    ApprovalError::Malformed("response missing `decision`".into())
                })?)
            }
            // MCP and Dynamic tools (Read/Glob/Grep/Task/...) — no native codex
            // approval shape. v2 default is to use the command-execution
            // requestApproval shape with a synthetic command string so the user
            // still sees what's happening; v3 should mint a generic shape.
            CodexToolKind::Mcp { server, tool } => {
                let synthesized = format!("MCP {server}.{tool}({})", req.input);
                let params = CommandExecutionRequestApprovalParams {
                    thread_id: thread_id.to_string(),
                    turn_id: turn_id.to_string(),
                    item_id: item_id.clone(),
                    command: Some(synthesized),
                    reason: req.decision_reason.clone(),
                    ..Default::default()
                };
                let value = send_server_request(
                    state,
                    "item/commandExecution/requestApproval",
                    serde_json::to_value(&params)
                        .map_err(|e| ApprovalError::Malformed(format!("encode params: {e}")))?,
                    None,
                )
                .await?;
                bucket_decision(value.get("decision").ok_or_else(|| {
                    ApprovalError::Malformed("response missing `decision`".into())
                })?)
            }
            // Dynamic + new semantic kinds (PlanExit, RequestUserInput,
            // Subagent, Exploration*, WebSearch, TodoUpdate) all share the
            // same approval path for now — synthesize a command-execution
            // request using the original tool name. Sections C/D/E/F may
            // later route some of these (eg AskUserQuestion) to a different
            // approval path; for now treat them uniformly.
            CodexToolKind::Dynamic { namespace, tool } => {
                let ns = namespace.as_deref().unwrap_or("");
                let synthesized = if ns.is_empty() {
                    format!("{tool}({})", req.input)
                } else {
                    format!("{ns}::{tool}({})", req.input)
                };
                let params = CommandExecutionRequestApprovalParams {
                    thread_id: thread_id.to_string(),
                    turn_id: turn_id.to_string(),
                    item_id: item_id.clone(),
                    command: Some(synthesized),
                    reason: req.decision_reason.clone(),
                    ..Default::default()
                };
                let value = send_server_request(
                    state,
                    "item/commandExecution/requestApproval",
                    serde_json::to_value(&params)
                        .map_err(|e| ApprovalError::Malformed(format!("encode params: {e}")))?,
                    None,
                )
                .await?;
                bucket_decision(value.get("decision").ok_or_else(|| {
                    ApprovalError::Malformed("response missing `decision`".into())
                })?)
            }
            CodexToolKind::PlanExit
            | CodexToolKind::RequestUserInput
            | CodexToolKind::Subagent
            | CodexToolKind::ExplorationRead
            | CodexToolKind::ExplorationSearch
            | CodexToolKind::ExplorationList
            | CodexToolKind::WebSearch
            | CodexToolKind::TodoUpdate => {
                let synthesized = format!("claude::{}({})", req.tool_name, req.input);
                let params = CommandExecutionRequestApprovalParams {
                    thread_id: thread_id.to_string(),
                    turn_id: turn_id.to_string(),
                    item_id: item_id.clone(),
                    command: Some(synthesized),
                    reason: req.decision_reason.clone(),
                    ..Default::default()
                };
                let value = send_server_request(
                    state,
                    "item/commandExecution/requestApproval",
                    serde_json::to_value(&params)
                        .map_err(|e| ApprovalError::Malformed(format!("encode params: {e}")))?,
                    None,
                )
                .await?;
                bucket_decision(value.get("decision").ok_or_else(|| {
                    ApprovalError::Malformed("response missing `decision`".into())
                })?)
            }
        };

    let payload = match outcome {
        ApprovalOutcome::Approved => json!({"behavior": "allow", "updatedInput": req.input}),
        ApprovalOutcome::Declined => json!({
            "behavior": "deny",
            "message": "user declined this tool call"
        }),
        ApprovalOutcome::Cancelled => json!({
            "behavior": "deny",
            "message": "user cancelled the turn",
            "interrupt": true
        }),
    };

    let envelope = ClaudeInbound::ControlResponse(OutboundControlResponseEnvelope {
        response: OutboundControlResponseInner::Success {
            request_id: req.request_id.clone(),
            response: Some(payload),
        },
    });
    if let Err(e) = handle.send_serialized(&envelope) {
        tracing::warn!(?e, "failed to send control_response back to claude");
    }
    Ok(outcome)
}

/// Send an error-shaped `control_response` for an inbound control_request the
/// bridge couldn't satisfy. Used when codex returns malformed data, the
/// client closes mid-flight, or the bridge can't classify the tool.
pub fn reply_control_error(
    handle: &Arc<ClaudeProcessHandle>,
    request_id: &str,
    message: impl Into<String>,
) {
    let envelope = ClaudeInbound::ControlResponse(OutboundControlResponseEnvelope {
        response: OutboundControlResponseInner::Error {
            request_id: request_id.to_string(),
            error: message.into(),
        },
    });
    if let Err(e) = handle.send_serialized(&envelope) {
        tracing::warn!(?e, "failed to send error control_response to claude");
    }
}

/// Send `item/tool/requestUserInput` to the codex client and await the
/// per-question answers. Mirrors pi-bridge's
/// [`alleycat_pi_bridge::approval::request_user_input`] (file:
/// `crates/pi-bridge/src/approval.rs:286`). The caller is responsible for
/// translating the returned `ToolRequestUserInputAnswer` map back into
/// the source process's expected reply payload (for the claude bridge,
/// that means a synthetic `tool_result` envelope on stdin).
pub async fn request_user_input(
    state: &Arc<ConnectionState>,
    thread_id: String,
    turn_id: String,
    item_id: String,
    questions: Vec<ToolRequestUserInputQuestion>,
    timeout: Option<Duration>,
) -> Result<std::collections::HashMap<String, ToolRequestUserInputAnswer>, ApprovalError> {
    let params = ToolRequestUserInputParams {
        thread_id,
        turn_id,
        item_id,
        questions,
    };
    let value = send_server_request(
        state,
        "item/tool/requestUserInput",
        serde_json::to_value(&params)
            .map_err(|e| ApprovalError::Malformed(format!("encode params: {e}")))?,
        timeout,
    )
    .await?;
    let resp: ToolRequestUserInputResponse = serde_json::from_value(value)
        .map_err(|e| ApprovalError::Malformed(format!("decode response: {e}")))?;
    Ok(resp.answers)
}

/// Low-level helper: register a pending id, send the request, await the
/// response under a deadline, clean up on timeout. Returns the raw
/// `result` JSON. Mirrors pi-bridge's `send_server_request`.
async fn send_server_request(
    state: &Arc<ConnectionState>,
    method: &str,
    params: Value,
    timeout: Option<Duration>,
) -> Result<Value, ApprovalError> {
    let deadline = timeout.unwrap_or(DEFAULT_APPROVAL_TIMEOUT);
    // Session-scoped id so reattach-replay can rebuild the outstanding-prompt
    // set deterministically across iroh disconnects.
    let req_id_str = state.session().next_request_id();
    let req_id = RequestId::String(req_id_str);
    let rx = state
        .register_pending_request(req_id.clone(), method.to_string(), params.clone())
        .await;

    let frame = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: JsonRpcVersion,
        id: req_id.clone(),
        method: method.to_string(),
        params: Some(params),
    });
    state
        .send(frame)
        .map_err(|_| ApprovalError::ConnectionClosed)?;

    match tokio::time::timeout(deadline, rx).await {
        Ok(Ok(Ok(value))) => Ok(value),
        Ok(Ok(Err(err))) => Err(err.into()),
        Ok(Err(_)) => Err(ApprovalError::ConnectionClosed),
        Err(_) => {
            // Reclaim the pending slot so a late response doesn't leak.
            let _ = state
                .resolve_pending_request(&req_id, Err(ServerRequestError::TimedOut))
                .await;
            Err(ApprovalError::Timeout(deadline))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_can_use_tool_request() {
        let v = json!({
            "request_id": "r1",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Bash",
                "input": {"command": "ls"},
                "tool_use_id": "toolu_42",
                "blocked_path": "/tmp/x",
                "decision_reason": "outside add-dir"
            }
        });
        let parsed = parse_can_use_tool(&v).expect("parsed");
        assert_eq!(parsed.request_id, "r1");
        assert_eq!(parsed.tool_name, "Bash");
        assert_eq!(parsed.input["command"], "ls");
        assert_eq!(parsed.tool_use_id.as_deref(), Some("toolu_42"));
        assert_eq!(parsed.blocked_path.as_deref(), Some("/tmp/x"));
    }

    #[test]
    fn rejects_non_can_use_tool_subtype() {
        let v = json!({
            "request_id": "r1",
            "request": {"subtype": "hook_callback"}
        });
        assert!(parse_can_use_tool(&v).is_none());
    }

    #[test]
    fn rejects_missing_fields() {
        // Missing tool_name
        let v = json!({
            "request_id": "r1",
            "request": {"subtype": "can_use_tool"}
        });
        assert!(parse_can_use_tool(&v).is_none());
    }

    #[test]
    fn bucketing_decisions() {
        assert_eq!(bucket_decision(&json!("accept")), ApprovalOutcome::Approved);
        assert_eq!(
            bucket_decision(&json!("acceptForSession")),
            ApprovalOutcome::Approved
        );
        assert_eq!(
            bucket_decision(&json!("decline")),
            ApprovalOutcome::Declined
        );
        assert_eq!(
            bucket_decision(&json!("cancel")),
            ApprovalOutcome::Cancelled
        );
        assert_eq!(
            bucket_decision(&json!("nonsense")),
            ApprovalOutcome::Declined
        );
        assert_eq!(
            bucket_decision(&json!({"acceptWithExecpolicyAmendment": {}})),
            ApprovalOutcome::Approved
        );
        assert_eq!(bucket_decision(&json!(42)), ApprovalOutcome::Declined);
    }
}
