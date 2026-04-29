//! Schema layer — every frame must round-trip through the typed
//! `codex-proto` structs that codex itself uses, and we capture a
//! key-fingerprint of each frame for the diff layer.

use std::collections::BTreeSet;

use serde_json::Value;

use alleycat_codex_proto as proto;

use crate::{Frame, FrameKind};

/// Result of validating one frame against the `codex-proto` schema.
#[derive(Debug, Clone)]
pub struct SchemaCheck {
    /// Was this frame an error response? Errors are not failures of the
    /// schema layer (they may be expected, see `KnownDivergence`); we expose
    /// the code so the diff layer can decide.
    pub error_code: Option<i64>,
    pub error_message: Option<String>,
    /// `Some(err)` if typed deserialize failed. The error string contains
    /// serde's path to the offending field.
    pub deserialize_error: Option<String>,
    /// Key fingerprint computed from the frame's "interesting" payload —
    /// `result` for responses, `params` for notifications. Sorted, so two
    /// fingerprints can be diffed deterministically.
    pub fingerprint: BTreeSet<String>,
}

impl SchemaCheck {
    pub fn ok(&self) -> bool {
        self.deserialize_error.is_none()
    }

    pub fn is_error_response(&self) -> bool {
        self.error_code.is_some() || self.error_message.is_some()
    }
}

pub fn check(frame: &Frame) -> SchemaCheck {
    match frame.kind {
        FrameKind::Response => check_response(&frame.method, &frame.raw),
        FrameKind::Notification => check_notification(&frame.raw),
    }
}

fn check_response(method: &str, raw: &Value) -> SchemaCheck {
    let mut out = SchemaCheck {
        error_code: None,
        error_message: None,
        deserialize_error: None,
        fingerprint: BTreeSet::new(),
    };

    if let Some(err) = raw.get("error") {
        out.error_code = err.get("code").and_then(Value::as_i64);
        out.error_message = err
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string);
        // Errors don't get a typed-shape check; diff layer interprets them.
        return out;
    }

    let result = raw.get("result").cloned().unwrap_or(Value::Null);
    out.fingerprint = fingerprint(&result);

    if let Err(err) = decode_response(method, &result) {
        out.deserialize_error = Some(err);
    }
    out
}

fn check_notification(raw: &Value) -> SchemaCheck {
    let mut out = SchemaCheck {
        error_code: None,
        error_message: None,
        deserialize_error: None,
        fingerprint: BTreeSet::new(),
    };
    let params = raw.get("params").cloned().unwrap_or(Value::Null);
    out.fingerprint = fingerprint(&params);

    // ServerNotification is `tag="method", content="params"`. Reconstruct a
    // {method, params} object so serde routes to the right variant.
    let method = raw.get("method").cloned().unwrap_or(Value::Null);
    let envelope = serde_json::json!({ "method": method, "params": params });
    if let Err(err) = serde_json::from_value::<proto::ServerNotification>(envelope) {
        out.deserialize_error = Some(format!("ServerNotification: {err}"));
    }
    out
}

fn decode_response(method: &str, result: &Value) -> Result<(), String> {
    macro_rules! decode {
        ($ty:ty) => {{
            serde_json::from_value::<$ty>(result.clone())
                .map(|_| ())
                .map_err(|e| format!("{}: {e}", stringify!($ty)))
        }};
    }
    match method {
        "initialize" => decode!(proto::InitializeResponse),
        "config/read" => decode!(proto::ConfigReadResponse),
        "config/value/write" => decode!(proto::ConfigWriteResponse),
        "config/batchWrite" => decode!(proto::ConfigWriteResponse),
        "configRequirements/read" => decode!(proto::ConfigRequirementsReadResponse),
        "model/list" => decode!(proto::ModelListResponse),
        "experimentalFeature/list" => decode!(proto::ExperimentalFeatureListResponse),
        "collaborationMode/list" => decode!(proto::CollaborationModeListResponse),
        "mcpServerStatus/list" => decode!(proto::ListMcpServerStatusResponse),
        "config/mcpServer/reload" => decode!(proto::McpServerRefreshResponse),
        "mcpServer/oauth/login" => decode!(proto::McpServerOauthLoginResponse),
        "skills/list" => decode!(proto::SkillsListResponse),
        "skills/remote/list" => Ok(()), // wire shape not pinned in proto
        "skills/remote/export" => Ok(()),
        "skills/config/write" => decode!(proto::SkillsConfigWriteResponse),
        "account/read" => decode!(proto::GetAccountResponse),
        "account/rateLimits/read" => decode!(proto::GetAccountRateLimitsResponse),
        "account/login/start" => decode!(proto::LoginAccountResponse),
        "account/login/cancel" => decode!(proto::CancelLoginAccountResponse),
        "account/logout" => decode!(proto::LogoutAccountResponse),
        "feedback/upload" => decode!(proto::FeedbackUploadResponse),
        "thread/start" => decode!(proto::ThreadStartResponse),
        "thread/resume" => decode!(proto::ThreadResumeResponse),
        "thread/fork" => decode!(proto::ThreadForkResponse),
        "thread/read" => decode!(proto::ThreadReadResponse),
        "thread/list" => decode!(proto::ThreadListResponse),
        "thread/loaded/list" => decode!(proto::ThreadLoadedListResponse),
        "thread/archive" => decode!(proto::ThreadArchiveResponse),
        "thread/unarchive" => decode!(proto::ThreadUnarchiveResponse),
        "thread/name/set" => decode!(proto::ThreadSetNameResponse),
        "thread/compact/start" => decode!(proto::ThreadCompactStartResponse),
        "thread/rollback" => decode!(proto::ThreadRollbackResponse),
        "thread/turns/list" => decode!(proto::ThreadTurnsListResponse),
        "thread/backgroundTerminals/clean" => {
            decode!(proto::ThreadBackgroundTerminalsCleanResponse)
        }
        "turn/start" => decode!(proto::TurnStartResponse),
        "turn/steer" => decode!(proto::TurnSteerResponse),
        "turn/interrupt" => decode!(proto::TurnInterruptResponse),
        "review/start" => decode!(proto::ReviewStartResponse),
        "command/exec" => decode!(proto::CommandExecResponse),
        "command/exec/write" => decode!(proto::CommandExecWriteResponse),
        "command/exec/terminate" => decode!(proto::CommandExecTerminateResponse),
        "command/exec/resize" => decode!(proto::CommandExecResizeResponse),
        "mock/experimentalMethod" => decode!(proto::MockExperimentalMethodResponse),
        // Unknown method — accept any shape; the diff layer will flag if
        // codex returned something but a bridge didn't.
        _ => Ok(()),
    }
}

/// Walk a JSON value and emit the dotted set of paths that are populated
/// with non-null content. Arrays use `[]` and recurse into the first
/// element so we capture the *shape* of list entries without enumerating
/// length-dependent indices.
pub fn fingerprint(value: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    walk(value, "", &mut out);
    out
}

fn walk(v: &Value, prefix: &str, out: &mut BTreeSet<String>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                if child.is_null() {
                    continue;
                }
                let path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                out.insert(path.clone());
                walk(child, &path, out);
            }
        }
        Value::Array(items) => {
            let path = format!("{prefix}[]");
            out.insert(path.clone());
            // Union the inner-key shapes across every element so we catch
            // optional fields that only appear on some entries.
            for item in items {
                walk(item, &path, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fingerprint_simple_object() {
        let v = json!({ "a": 1, "b": { "c": "x" } });
        let fp = fingerprint(&v);
        assert!(fp.contains("a"));
        assert!(fp.contains("b"));
        assert!(fp.contains("b.c"));
    }

    #[test]
    fn fingerprint_array_unions_keys_across_elements() {
        let v = json!({ "items": [ { "x": 1 }, { "y": 2 } ] });
        let fp = fingerprint(&v);
        assert!(fp.contains("items[]"));
        assert!(fp.contains("items[].x"));
        assert!(fp.contains("items[].y"));
    }

    #[test]
    fn fingerprint_skips_null_fields() {
        let v = json!({ "a": null, "b": 1 });
        let fp = fingerprint(&v);
        assert!(!fp.contains("a"));
        assert!(fp.contains("b"));
    }
}
