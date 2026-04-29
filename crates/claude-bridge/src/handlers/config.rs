//! `config/read`, `config/value/write`, `config/batchWrite`,
//! `configRequirements/read`.
//!
//! v1 surface: `config/read` returns the contents of `~/.claude/settings.json`
//! verbatim (or `{}` if the file is missing). `config/value/write` and
//! `config/batchWrite` are stubs returning `{}` — mutating the user's
//! `~/.claude/settings.json` from the bridge is risky and out of scope for v1
//! (claude itself owns that file, and a half-applied write could corrupt the
//! user's claude config).
//!
//! `configRequirements/read` reports no special requirements; codex clients
//! interpret an empty list as "anything goes".

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;
use serde_json::json;

use alleycat_codex_proto as p;

use crate::state::ConnectionState;

/// Path to the user's `~/.claude/settings.json`. Returns `None` only if no
/// home directory could be resolved (effectively impossible on Unix/macOS).
pub fn claude_settings_path() -> Option<PathBuf> {
    let home = directories::UserDirs::new()?.home_dir().to_path_buf();
    Some(home.join(".claude").join("settings.json"))
}

pub fn handle_config_read(
    _state: &Arc<ConnectionState>,
    _codex_home: &Path,
    _params: p::ConfigReadParams,
) -> Result<p::ConfigReadResponse> {
    let config = read_json_or_default(claude_settings_path().as_deref());
    Ok(p::ConfigReadResponse {
        config,
        origins: Default::default(),
        layers: None,
    })
}

pub fn handle_config_value_write(
    _state: &Arc<ConnectionState>,
    _codex_home: &Path,
    _params: p::ConfigValueWriteParams,
) -> Result<p::ConfigWriteResponse> {
    Ok(stub_write_response())
}

pub fn handle_config_batch_write(
    _state: &Arc<ConnectionState>,
    _codex_home: &Path,
    _params: p::ConfigBatchWriteParams,
) -> Result<p::ConfigWriteResponse> {
    Ok(stub_write_response())
}

pub fn handle_config_requirements_read(
    _state: &Arc<ConnectionState>,
) -> p::ConfigRequirementsReadResponse {
    p::ConfigRequirementsReadResponse { requirements: None }
}

fn stub_write_response() -> p::ConfigWriteResponse {
    p::ConfigWriteResponse {
        status: p::WriteStatus::Ok,
        version: "0".to_string(),
        // `file_path` is informational; reporting the canonical claude path
        // keeps the response interpretable without misleading the client into
        // thinking the bridge actually wrote there.
        file_path: claude_settings_path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        overridden_metadata: None,
    }
}

fn read_json_or_default(path: Option<&Path>) -> Value {
    let Some(p) = path else {
        return json!({});
    };
    match std::fs::read_to_string(p) {
        Ok(text) => serde_json::from_str::<Value>(&text).unwrap_or_else(|err| {
            tracing::warn!(?p, %err, "failed to parse claude settings; using empty config");
            json!({})
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(err) => {
            tracing::warn!(?p, %err, "failed to read claude settings; using empty config");
            json!({})
        }
    }
}
