//! `config/read`, `config/value/write`, `config/batchWrite`,
//! `configRequirements/read`. The bridge composes pi's settings.json with a
//! small set of bridge-only keys (approval_policy, sandbox, defaults).

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use serde_json::Value;
use serde_json::json;

use crate::codex_proto as p;
use crate::state::ConnectionState;

/// Path to pi's persistent settings file. See pi-mono
/// `core/settings-manager.ts` — the bridge mirrors that location and treats
/// missing files as "no overrides".
pub fn pi_settings_path() -> Option<PathBuf> {
    let home = directories::UserDirs::new()?.home_dir().to_path_buf();
    Some(home.join(".pi").join("agent").join("settings.json"))
}

/// Path to the bridge's own config file (`config.json` under the codex_home
/// the bridge advertised at `initialize`).
pub fn bridge_config_path(codex_home: &Path) -> PathBuf {
    codex_home.join("config.json")
}

pub fn handle_config_read(
    _state: &Arc<ConnectionState>,
    codex_home: &Path,
    _params: p::ConfigReadParams,
) -> Result<p::ConfigReadResponse> {
    let pi = read_json_or_default(pi_settings_path().as_deref());
    let bridge = read_json_or_default(Some(&bridge_config_path(codex_home)));
    let merged = merge_config(&pi, &bridge);

    Ok(p::ConfigReadResponse {
        config: merged,
        origins: HashMap::new(),
        layers: None,
    })
}

pub fn handle_config_value_write(
    _state: &Arc<ConnectionState>,
    codex_home: &Path,
    params: p::ConfigValueWriteParams,
) -> Result<p::ConfigWriteResponse> {
    let path = bridge_config_path(codex_home);
    write_keys(&path, &[(params.key_path.clone(), params.value.clone())])?;

    // TODO(pi-runtime): if `key_path` corresponds to a pi setting (model,
    // thinking, queue modes, auto-compaction, auto-retry), forward to the
    // owning pi process via `state.pi_pool()`. Stubbed for now — pool API
    // lands in pi-runtime task #7.

    Ok(p::ConfigWriteResponse {
        status: p::WriteStatus::Ok,
        version: "0".to_string(),
        file_path: path.to_string_lossy().into_owned(),
        overridden_metadata: None,
    })
}

pub fn handle_config_batch_write(
    _state: &Arc<ConnectionState>,
    codex_home: &Path,
    params: p::ConfigBatchWriteParams,
) -> Result<p::ConfigWriteResponse> {
    let path = bridge_config_path(codex_home);
    let edits: Vec<(String, Value)> = params
        .edits
        .iter()
        .map(|e| (e.key_path.clone(), e.value.clone()))
        .collect();
    write_keys(&path, &edits)?;

    Ok(p::ConfigWriteResponse {
        status: p::WriteStatus::Ok,
        version: "0".to_string(),
        file_path: path.to_string_lossy().into_owned(),
        overridden_metadata: None,
    })
}

pub fn handle_config_requirements_read(
    _state: &Arc<ConnectionState>,
) -> p::ConfigRequirementsReadResponse {
    // The bridge currently advertises no special requirements. Codex clients
    // treat `requirements: None` as "anything goes".
    p::ConfigRequirementsReadResponse { requirements: None }
}

// === helpers ==============================================================

fn read_json_or_default(path: Option<&Path>) -> Value {
    let Some(p) = path else {
        return json!({});
    };
    match std::fs::read_to_string(p) {
        Ok(text) => serde_json::from_str::<Value>(&text).unwrap_or_else(|err| {
            tracing::warn!(?p, %err, "failed to parse settings file; using empty config");
            json!({})
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(err) => {
            tracing::warn!(?p, %err, "failed to read settings file; using empty config");
            json!({})
        }
    }
}

/// Shallow object merge: bridge keys override pi keys when they collide.
/// Both sides are expected to be JSON objects.
fn merge_config(pi: &Value, bridge: &Value) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(obj) = pi.as_object() {
        for (k, v) in obj {
            out.insert(k.clone(), v.clone());
        }
    }
    if let Some(obj) = bridge.as_object() {
        for (k, v) in obj {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

/// Persist a set of `key_path → value` pairs into the bridge config file.
/// `key_path` follows codex's dotted convention (e.g. `approval_policy`,
/// `sandbox.network_access`).
fn write_keys(path: &Path, edits: &[(String, Value)]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating bridge config dir {parent:?}"))?;
    }
    let mut current = read_json_or_default(Some(path));
    for (key_path, value) in edits {
        set_dotted_key(&mut current, key_path, value.clone());
    }
    let text = serde_json::to_string_pretty(&current)?;
    std::fs::write(path, text).with_context(|| format!("writing bridge config {path:?}"))?;
    Ok(())
}

fn set_dotted_key(root: &mut Value, key_path: &str, value: Value) {
    let mut parts = key_path.split('.').peekable();
    let mut cursor = root;
    while let Some(part) = parts.next() {
        if !cursor.is_object() {
            *cursor = Value::Object(serde_json::Map::new());
        }
        let map = cursor.as_object_mut().expect("ensured object above");
        if parts.peek().is_none() {
            map.insert(part.to_string(), value);
            return;
        }
        cursor = map.entry(part.to_string()).or_insert_with(|| json!({}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_lets_bridge_override_pi() {
        let pi = json!({"model": "pi-default", "thinking": "low"});
        let bridge = json!({"model": "bridge-override", "approval_policy": "on-failure"});
        let out = merge_config(&pi, &bridge);
        assert_eq!(out["model"], json!("bridge-override"));
        assert_eq!(out["thinking"], json!("low"));
        assert_eq!(out["approval_policy"], json!("on-failure"));
    }

    #[test]
    fn dotted_key_writes_nested_path() {
        let mut root = json!({});
        set_dotted_key(&mut root, "sandbox.network_access", json!(true));
        set_dotted_key(&mut root, "model", json!("gpt-5"));
        assert_eq!(
            root,
            json!({"sandbox": {"network_access": true}, "model": "gpt-5"})
        );
    }
}
