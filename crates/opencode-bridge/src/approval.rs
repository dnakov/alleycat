//! Approval-flow plumbing — codex `item/{commandExecution,fileChange}/requestApproval`
//! ↔ opencode `permission.asked` / `POST /permission/{id}/reply`.
//!
//! Mirrors `crates/pi-bridge/src/approval.rs` but talks to opencode's
//! permission HTTP endpoint instead of pi's `RpcCommand::ApprovalResponse`.
//!
//! The split is deliberate:
//!
//! - This module owns the **codex side**: encoding the request to send to
//!   the codex client, awaiting the JSON-RPC response, and bucketing the
//!   decision tag into a small, stable enum.
//! - The **opencode side** (POSTing the reply) is a one-line method on
//!   `OpencodeClient` (`permission_reply`), called by the SSE event router
//!   after this module returns.

use std::time::Duration;

use alleycat_bridge_core::NotificationSender;
use alleycat_bridge_core::state::ServerRequestError;
use serde_json::Value;
use thiserror::Error;

/// Default time the bridge waits for a client approval before timing out and
/// treating the prompt as a `Decline`. 5 minutes matches pi-bridge's default
/// (`crates/pi-bridge/src/approval.rs:DEFAULT_APPROVAL_TIMEOUT`).
pub const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Bucketed view of codex's per-decision-type enums. The bridge only needs to
/// know whether the user wants to (a) allow this one call, (b) allow this
/// kind of call for the rest of the session, or (c) deny.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOutcome {
    /// Approve once — codex tags `accept` / `approved`.
    ApprovedOnce,
    /// Approve for the rest of the session — codex tags `acceptForSession`,
    /// `approvedForSession`, `acceptWithExecpolicyAmendment`, etc.
    ApprovedForSession,
    /// Decline — codex tags `decline` / `denied` / `cancel` / `abort` (we
    /// fold cancel into reject because the bridge has nothing to abort
    /// independently of what opencode will do on its own).
    Rejected,
}

impl ApprovalOutcome {
    /// Map to the opencode `Reply` literal for `POST /permission/{id}/reply`.
    pub fn as_opencode_reply(self) -> &'static str {
        match self {
            Self::ApprovedOnce => "once",
            Self::ApprovedForSession => "always",
            Self::Rejected => "reject",
        }
    }
}

/// Bucket the codex decision JSON into [`ApprovalOutcome`]. Codex serializes
/// the decision either as a bare string (`"accept"`, `"decline"`, ...) or as
/// a tagged object (`{ "acceptWithExecpolicyAmendment": { ... } }`). Unknown
/// tags bucket as `Rejected` to avoid silent approve.
pub fn bucket_decision(value: &Value) -> ApprovalOutcome {
    let tag = match value {
        Value::String(s) => s.as_str(),
        Value::Object(map) => match map.keys().next() {
            Some(k) => k.as_str(),
            None => return ApprovalOutcome::Rejected,
        },
        _ => return ApprovalOutcome::Rejected,
    };
    match tag {
        "accept" | "approved" | "approve" => ApprovalOutcome::ApprovedOnce,
        "acceptForSession"
        | "approvedForSession"
        | "acceptWithExecpolicyAmendment"
        | "applyNetworkPolicyAmendment" => ApprovalOutcome::ApprovedForSession,
        "decline" | "denied" | "deny" | "cancel" | "abort" => ApprovalOutcome::Rejected,
        other => {
            tracing::warn!(decision = %other, "unknown codex approval decision — treating as reject");
            ApprovalOutcome::Rejected
        }
    }
}

/// Errors the codex-side approval round trip can yield.
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
            ServerRequestError::Rpc(err) => Self::Rpc {
                code: err.code,
                message: err.message,
            },
            ServerRequestError::ConnectionClosed => Self::ConnectionClosed,
            ServerRequestError::TimedOut => Self::Timeout(DEFAULT_APPROVAL_TIMEOUT),
        }
    }
}

/// Send an approval request to the codex client and bucket the decision.
/// `method` should be `"item/commandExecution/requestApproval"` or
/// `"item/fileChange/requestApproval"` — the codex client returns a result
/// with a `decision` field for either.
pub async fn request_approval(
    notifier: &NotificationSender,
    method: &str,
    params: Value,
    timeout: Option<Duration>,
) -> Result<ApprovalOutcome, ApprovalError> {
    let timeout = timeout.unwrap_or(DEFAULT_APPROVAL_TIMEOUT);
    let value = notifier
        .request(method.to_string(), params, timeout)
        .await?;
    let decision = value
        .get("decision")
        .ok_or_else(|| ApprovalError::Malformed("response missing `decision`".into()))?;
    Ok(bucket_decision(decision))
}

/// Decide whether an opencode `permission.asked` payload describes a command
/// or a file change. opencode's `permission` string is e.g. `"bash"`,
/// `"write"`, `"edit"` — anything non-shell-shaped is treated as a file
/// change.
pub fn classify_permission(permission: &str, metadata: &Value) -> PermissionKind {
    // Heuristic: any metadata.command implies a shell-ish prompt; otherwise
    // map by the `permission` literal.
    if metadata.get("command").is_some() {
        return PermissionKind::Command;
    }
    match permission {
        "bash" | "shell" | "exec" | "run" => PermissionKind::Command,
        _ => PermissionKind::FileChange,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionKind {
    Command,
    FileChange,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bucket_string_decisions() {
        assert_eq!(
            bucket_decision(&json!("accept")),
            ApprovalOutcome::ApprovedOnce
        );
        assert_eq!(
            bucket_decision(&json!("approved")),
            ApprovalOutcome::ApprovedOnce
        );
        assert_eq!(
            bucket_decision(&json!("acceptForSession")),
            ApprovalOutcome::ApprovedForSession
        );
        assert_eq!(
            bucket_decision(&json!("approvedForSession")),
            ApprovalOutcome::ApprovedForSession
        );
        assert_eq!(bucket_decision(&json!("decline")), ApprovalOutcome::Rejected);
        assert_eq!(bucket_decision(&json!("denied")), ApprovalOutcome::Rejected);
        assert_eq!(bucket_decision(&json!("cancel")), ApprovalOutcome::Rejected);
        assert_eq!(
            bucket_decision(&json!("totallyMadeUp")),
            ApprovalOutcome::Rejected
        );
    }

    #[test]
    fn bucket_object_decisions() {
        let amendment = json!({"acceptWithExecpolicyAmendment": {}});
        assert_eq!(
            bucket_decision(&amendment),
            ApprovalOutcome::ApprovedForSession
        );
        assert_eq!(bucket_decision(&json!({})), ApprovalOutcome::Rejected);
    }

    #[test]
    fn approval_outcome_maps_to_opencode_reply() {
        assert_eq!(ApprovalOutcome::ApprovedOnce.as_opencode_reply(), "once");
        assert_eq!(
            ApprovalOutcome::ApprovedForSession.as_opencode_reply(),
            "always"
        );
        assert_eq!(ApprovalOutcome::Rejected.as_opencode_reply(), "reject");
    }

    #[test]
    fn classify_permission_treats_metadata_command_as_command() {
        let kind = classify_permission("write", &json!({"command":"rm -rf /"}));
        assert_eq!(kind, PermissionKind::Command);
    }

    #[test]
    fn classify_permission_uses_string_when_no_command() {
        assert_eq!(
            classify_permission("bash", &json!({})),
            PermissionKind::Command
        );
        assert_eq!(
            classify_permission("write", &json!({})),
            PermissionKind::FileChange
        );
        assert_eq!(
            classify_permission("edit", &json!({"path":"/x"})),
            PermissionKind::FileChange
        );
    }
}
