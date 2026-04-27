//! MCP-related stubs. The bridge does not proxy MCP servers in v1; these
//! handlers reply with empty success and emit a single `configWarning`
//! notification so the codex client knows MCP routing is intentionally
//! unavailable.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use crate::codex_proto as p;
use crate::state::ConnectionState;

/// Rate-limit the "MCP not bridged" warning to once per connection.
static MCP_WARNING_EMITTED: AtomicBool = AtomicBool::new(false);

fn emit_warning_once(state: &Arc<ConnectionState>) {
    if MCP_WARNING_EMITTED.swap(true, Ordering::Relaxed) {
        return;
    }
    if !state.should_emit("configWarning") {
        return;
    }
    let notif = p::ServerNotification::ConfigWarning(p::ConfigWarningNotification {
        summary: "MCP servers are not bridged in pi-bridge v1".to_string(),
        details: Some(
            "MCP-related methods (mcpServer/*) reply with empty success. \
             pi extensions are not exposed to codex clients yet."
                .to_string(),
        ),
        path: None,
        range: None,
    });
    let frame = match notification_message(&notif) {
        Ok(frame) => frame,
        Err(err) => {
            tracing::warn!(%err, "failed to encode MCP configWarning");
            return;
        }
    };
    let _ = state.send(frame);
}

fn notification_message(
    notif: &p::ServerNotification,
) -> Result<p::JsonRpcMessage, serde_json::Error> {
    let value = serde_json::to_value(notif)?;
    let method = value
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or_default()
        .to_string();
    let params = value.get("params").cloned();
    Ok(p::JsonRpcMessage::Notification(p::JsonRpcNotification {
        jsonrpc: p::JsonRpcVersion,
        method,
        params,
    }))
}

pub fn handle_mcp_server_status_list(
    state: &Arc<ConnectionState>,
    _params: p::ListMcpServerStatusParams,
) -> p::ListMcpServerStatusResponse {
    emit_warning_once(state);
    p::ListMcpServerStatusResponse::default()
}

pub fn handle_mcp_server_refresh(state: &Arc<ConnectionState>) -> p::McpServerRefreshResponse {
    emit_warning_once(state);
    p::McpServerRefreshResponse::default()
}

pub fn handle_mcp_server_oauth_login(
    state: &Arc<ConnectionState>,
    _params: p::McpServerOauthLoginParams,
) -> p::McpServerOauthLoginResponse {
    emit_warning_once(state);
    // Codex clients treat any URL here as opaque; an empty string is fine —
    // the matching `mcpServer/oauthLogin/completed` notification (which we
    // never emit) is what would carry success/failure.
    p::McpServerOauthLoginResponse {
        authorization_url: String::new(),
    }
}
