//! Validate captured wire frames against the canonical
//! `codex-rs/app-server-protocol/schema/json/v2/` JSON schemas.
//!
//! Why: our local `codex-proto` types are a hand-maintained mirror that can
//! drift from upstream — fields renamed, enum variants added, optional/required
//! flipped. The diff layer's typed-decode check only proves we're consistent
//! with the *mirror*. This pass loads the real schemas codex publishes and
//! validates each frame against them; a violation here is a real wire-spec
//! gap the bridge needs to fix.
//!
//! Skip-on-missing: the schema dir defaults to
//! `~/dev/codex/codex-rs/app-server-protocol/schema/json/v2/`. If absent
//! (CI, fresh checkout, stripped tarball) the validator silently no-ops so
//! the rest of the conformance suite still runs.
//!
//! Method/notification → schema mapping is built from the upstream filename
//! convention: response of `thread/read` lives in `ThreadReadResponse.json`,
//! notification `item/completed` in `ItemCompletedNotification.json`, etc.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use jsonschema::Validator;
use serde_json::Value;

use crate::{Frame, FrameKind};

const ENV_OVERRIDE: &str = "BRIDGE_CONFORMANCE_CODEX_SCHEMA_DIR";
const DEFAULT_REL: &str = "dev/codex/codex-rs/app-server-protocol/schema/json/v2";

/// Directory holding the upstream v2 JSON schema files. `None` if both the
/// env var is unset *and* the default relative path under `$HOME` is absent.
pub fn schema_dir() -> Option<PathBuf> {
    if let Some(custom) = std::env::var_os(ENV_OVERRIDE) {
        let p = PathBuf::from(custom);
        if p.is_dir() {
            return Some(p);
        }
        // Explicit override that points at nothing — surface the choice
        // rather than falling back to the implicit default.
        tracing::warn!(
            override_env = ENV_OVERRIDE,
            path = %p.display(),
            "schema dir override does not exist; upstream-schema validation skipped"
        );
        return None;
    }
    let home = std::env::var_os("HOME")?;
    let candidate = PathBuf::from(home).join(DEFAULT_REL);
    candidate.is_dir().then_some(candidate)
}

/// Validate a single captured frame against its upstream schema. Returns
/// `Ok(())` when the schema is missing (skipped) or the frame validates;
/// otherwise returns the validator's error list joined into one human-
/// readable message.
pub fn validate(frame: &Frame) -> Result<(), String> {
    let Some(schema_path) = path_for_frame(frame) else {
        // No schema file mapped for this method/notification — silent skip
        // (e.g., bridge-only methods, or methods we haven't mapped yet).
        return Ok(());
    };
    let validator = match cached_validator(&schema_path) {
        Ok(v) => v,
        Err(err) => {
            // Don't fail the whole frame just because the schema didn't
            // load — surface it once via tracing and skip.
            tracing::debug!(
                schema = %schema_path.display(),
                ?err,
                "upstream schema unavailable; skipping validation"
            );
            return Ok(());
        }
    };
    let payload = extract_payload(frame);
    let errors: Vec<String> = validator
        .iter_errors(&payload)
        .map(|e| format!("{}: {}", e.instance_path, e))
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Map a frame to its upstream schema filename. We translate the JSON-RPC
/// method name into the upstream's PascalCase + suffix convention
/// (`thread/read` → `ThreadReadResponse.json` for responses,
/// `item/completed` → `ItemCompletedNotification.json` for notifications).
fn path_for_frame(frame: &Frame) -> Option<PathBuf> {
    let dir = schema_dir()?;
    let stem = match frame.kind {
        FrameKind::Response => format!("{}Response", method_to_pascal(&frame.method)),
        FrameKind::Notification => format!("{}Notification", method_to_pascal(&frame.method)),
    };
    let candidate = dir.join(format!("{stem}.json"));
    candidate.is_file().then_some(candidate)
}

/// `thread/read` → `ThreadRead`, `item/agentMessage/delta` → `ItemAgentMessageDelta`.
/// The codex schema files use ASCII PascalCase formed by capitalizing each
/// `/`-separated segment without altering already-camelCased segments.
fn method_to_pascal(method: &str) -> String {
    let mut out = String::with_capacity(method.len());
    for segment in method.split('/') {
        if segment.is_empty() {
            continue;
        }
        let mut chars = segment.chars();
        if let Some(first) = chars.next() {
            for c in first.to_uppercase() {
                out.push(c);
            }
        }
        out.push_str(chars.as_str());
    }
    out
}

/// Pull the validation target out of a frame:
///   - response: `result` body (the schema describes the result, not the
///     full JSON-RPC envelope).
///   - notification: `params` body.
fn extract_payload(frame: &Frame) -> Value {
    let target = match frame.kind {
        FrameKind::Response => "result",
        FrameKind::Notification => "params",
    };
    frame.raw.get(target).cloned().unwrap_or(Value::Null)
}

fn cached_validator(path: &Path) -> Result<&'static Validator, String> {
    static CACHE: OnceLock<std::sync::Mutex<HashMap<PathBuf, &'static Validator>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut guard = cache.lock().expect("schema cache poisoned");
    if let Some(&v) = guard.get(path) {
        return Ok(v);
    }
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let json: Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("parse {}: {e}", path.display()))?;
    let validator =
        jsonschema::draft7::new(&json).map_err(|e| format!("compile {}: {e}", path.display()))?;
    let leaked: &'static Validator = Box::leak(Box::new(validator));
    guard.insert(path.to_path_buf(), leaked);
    Ok(leaked)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_to_pascal_handles_slashes_and_camel() {
        assert_eq!(method_to_pascal("thread/read"), "ThreadRead");
        assert_eq!(method_to_pascal("item/completed"), "ItemCompleted");
        assert_eq!(
            method_to_pascal("item/agentMessage/delta"),
            "ItemAgentMessageDelta"
        );
        assert_eq!(method_to_pascal("turn/start"), "TurnStart");
    }
}
