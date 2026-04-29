//! `skills/list` and the `skills/remote/*` + `skills/config/write` no-ops.
//!
//! `skills/list` translates pi `get_commands` results into codex
//! `SkillMetadata`. Pi's command catalog includes three sources —
//! `extension` (pi extension actions), `prompt` (saved prompt templates),
//! and `skill` (the SKILL.md/SKILL.json catalog). Per the design plan, only
//! `source:"skill"` entries surface to codex; the other two are pi-internal
//! UI affordances that don't map onto codex's skill model.
//!
//! Pi prefixes skill commands with `skill:<name>` (see
//! `pi-mono/.../rpc-mode.ts:624`); the bridge strips that prefix before
//! emitting `SkillMetadata.name` so codex sees the bare skill name.
//!
//! Each `cwd` in the request maps to one pi process via
//! [`crate::pool::PiPool::acquire_utility(Some(cwd))`]: the pool reuses
//! any existing thread-bound pi for that cwd, otherwise spawns a
//! short-lived utility pi that the next idle reap sweeps. On RPC failure
//! the entry's `errors` list carries the message and `skills` is empty;
//! codex clients tolerate per-cwd partial failures.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use crate::codex_proto as p;
use crate::pool::pi_protocol as pi;
use crate::state::ConnectionState;

/// Pi prefixes skill commands with `skill:<name>` in `get_commands`. The
/// bridge surfaces the bare skill name to codex.
const PI_SKILL_PREFIX: &str = "skill:";

pub async fn handle_skills_list(
    state: &Arc<ConnectionState>,
    params: p::SkillsListParams,
) -> p::SkillsListResponse {
    let cwds = if params.cwds.is_empty() {
        vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))]
    } else {
        params.cwds
    };

    let mut data = Vec::with_capacity(cwds.len());
    for cwd in cwds {
        let (pi_commands, errors) = fetch_commands_via_pool(state, &cwd).await;
        let skills = pi_commands
            .into_iter()
            .filter_map(translate_skill_command)
            .collect();
        data.push(p::SkillsListEntry {
            cwd,
            skills,
            errors,
        });
    }
    p::SkillsListResponse { data }
}

/// `skills/remote/list` is a no-op: the bridge does not consult any remote
/// skill registry today.
pub async fn handle_skills_remote_list(_state: &Arc<ConnectionState>) -> Value {
    serde_json::json!({ "data": [] })
}

/// `skills/remote/export` is a no-op success.
pub async fn handle_skills_remote_export(_state: &Arc<ConnectionState>, _params: Value) -> Value {
    serde_json::json!({})
}

/// `skills/config/write` round-trips the `enabled` flag the client sent so
/// the codex UI doesn't error out when toggling a skill — the bridge does
/// not persist this preference today.
pub async fn handle_skills_config_write(
    _state: &Arc<ConnectionState>,
    params: p::SkillsConfigWriteParams,
) -> p::SkillsConfigWriteResponse {
    p::SkillsConfigWriteResponse {
        effective_enabled: params.enabled,
    }
}

// === pool integration =====================================================

/// Fetch the slash-command catalog from a pi process bound to `cwd`.
/// Returns `(commands, errors)` — `errors` is one-element on spawn or RPC
/// failure (carrying the message keyed off `cwd`), empty on success.
async fn fetch_commands_via_pool(
    state: &Arc<ConnectionState>,
    cwd: &std::path::Path,
) -> (Vec<PiSlashCommand>, Vec<p::SkillErrorInfo>) {
    let handle = match state.pi_pool().acquire_utility(Some(cwd)).await {
        Ok(h) => h,
        Err(err) => {
            tracing::warn!(?cwd, %err, "skills/list: failed to acquire utility pi");
            return (
                Vec::new(),
                vec![p::SkillErrorInfo {
                    path: cwd.to_path_buf(),
                    message: format!("pi process unavailable: {err}"),
                }],
            );
        }
    };
    match fetch_commands_from_handle(&handle).await {
        Ok(commands) => (commands, Vec::new()),
        Err(err) => {
            tracing::warn!(?cwd, %err, "skills/list: get_commands RPC failed");
            (
                Vec::new(),
                vec![p::SkillErrorInfo {
                    path: cwd.to_path_buf(),
                    message: format!("pi get_commands failed: {err}"),
                }],
            )
        }
    }
}

/// Drive `RpcCommand::GetCommands` against a live pi handle. Free function
/// so tests can exercise the unpacking layer without standing up the full
/// pool.
async fn fetch_commands_from_handle(
    handle: &crate::pool::PiProcessHandle,
) -> Result<Vec<PiSlashCommand>, crate::pool::PiProcessError> {
    let resp = handle
        .send_request(pi::RpcCommand::GetCommands(pi::BareCmd::default()))
        .await?;
    let value = resp.data.unwrap_or(Value::Null);
    Ok(parse_pi_commands_response(&value))
}

fn parse_pi_commands_response(value: &Value) -> Vec<PiSlashCommand> {
    let Some(commands) = value.get("commands").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    commands
        .iter()
        .filter_map(|c| serde_json::from_value(c.clone()).ok())
        .collect()
}

// === translation ==========================================================

/// Filter to `source:"skill"`, strip the `skill:` prefix from `name`, and
/// build a `SkillMetadata` payload. Returns `None` for non-skill sources
/// (`extension`, `prompt`) which pi exposes but codex's skill catalog does
/// not.
fn translate_skill_command(cmd: PiSlashCommand) -> Option<p::SkillMetadata> {
    if !matches!(cmd.source, PiCommandSource::Skill) {
        return None;
    }
    let name = cmd
        .name
        .strip_prefix(PI_SKILL_PREFIX)
        .map(str::to_string)
        .unwrap_or(cmd.name);
    let description = cmd.description.unwrap_or_default();
    // Pi's `sourceInfo.path` is the on-disk SKILL.md / SKILL.json path. Codex
    // wants the absolute path; we round-trip whatever pi gave us. Empty
    // string when pi has no path (defensive — shouldn't happen for skills).
    let path = cmd.source_info.and_then(|si| si.path).unwrap_or_default();
    Some(p::SkillMetadata {
        name,
        description,
        short_description: None,
        interface: None,
        dependencies: None,
        path,
        scope: p::SkillScope::Repo,
        enabled: true,
    })
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PiSlashCommand {
    name: String,
    #[serde(default)]
    description: Option<String>,
    source: PiCommandSource,
    #[serde(default)]
    source_info: Option<PiSourceInfo>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum PiCommandSource {
    Extension,
    Prompt,
    Skill,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PiSourceInfo {
    #[serde(default)]
    path: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn translate_drops_prefix_and_marks_repo_scope() {
        let cmd = PiSlashCommand {
            name: "skill:rotate-keys".into(),
            description: Some("Rotate AWS keys".into()),
            source: PiCommandSource::Skill,
            source_info: Some(PiSourceInfo {
                path: Some("/repo/.pi/skills/rotate-keys/SKILL.md".into()),
            }),
        };
        let m = translate_skill_command(cmd).expect("skill should translate");
        assert_eq!(m.name, "rotate-keys");
        assert_eq!(m.description, "Rotate AWS keys");
        assert_eq!(m.path, "/repo/.pi/skills/rotate-keys/SKILL.md");
        assert!(matches!(m.scope, p::SkillScope::Repo));
        assert!(m.enabled);
    }

    #[test]
    fn translate_keeps_name_when_prefix_missing() {
        let cmd = PiSlashCommand {
            name: "deploy".into(),
            description: None,
            source: PiCommandSource::Skill,
            source_info: None,
        };
        let m = translate_skill_command(cmd).unwrap();
        assert_eq!(m.name, "deploy");
        assert_eq!(m.description, "");
        assert_eq!(m.path, "");
    }

    #[test]
    fn translate_drops_extension_and_prompt_sources() {
        for source in [PiCommandSource::Extension, PiCommandSource::Prompt] {
            let cmd = PiSlashCommand {
                name: "anything".into(),
                description: None,
                source,
                source_info: None,
            };
            assert!(translate_skill_command(cmd).is_none());
        }
    }

    #[test]
    fn parse_pi_commands_response_extracts_array() {
        let raw = json!({
            "commands": [
                {
                    "name": "skill:rotate-keys",
                    "description": "Rotate AWS keys",
                    "source": "skill",
                    "sourceInfo": { "path": "/repo/.pi/skills/rotate-keys/SKILL.md" },
                },
                { "name": "deploy", "source": "extension" },
                { "name": "review-pr", "source": "prompt" },
            ]
        });
        let parsed = parse_pi_commands_response(&raw);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].source, PiCommandSource::Skill);
        assert_eq!(parsed[1].source, PiCommandSource::Extension);
        assert_eq!(parsed[2].source, PiCommandSource::Prompt);
    }

    async fn dummy_state() -> Arc<ConnectionState> {
        let dir = tempfile::tempdir().unwrap();
        let index = crate::index::ThreadIndex::open_at(dir.path().join("threads.json"))
            .await
            .unwrap();
        std::mem::forget(dir);
        let (state, _rx) = ConnectionState::for_test(
            Arc::new(crate::pool::PiPool::new("/dev/null")),
            index,
            Default::default(),
        );
        state
    }

    #[tokio::test]
    async fn skills_list_returns_one_entry_per_cwd() {
        let state = dummy_state().await;
        let params = p::SkillsListParams {
            cwds: vec![PathBuf::from("/x"), PathBuf::from("/y")],
            ..Default::default()
        };
        let resp = handle_skills_list(&state, params).await;
        assert_eq!(resp.data.len(), 2);
        assert_eq!(resp.data[0].cwd, PathBuf::from("/x"));
        assert!(resp.data[0].skills.is_empty());
        assert_eq!(resp.data[1].cwd, PathBuf::from("/y"));
        // The dummy pool's pi_bin is `/dev/null`, so utility spawns fail
        // and each entry reports a per-cwd error. Real deployments use a
        // valid pi binary and these errors are empty on success.
        assert_eq!(resp.data[0].errors.len(), 1);
        assert_eq!(resp.data[0].errors[0].path, PathBuf::from("/x"));
        assert!(
            resp.data[0].errors[0]
                .message
                .contains("pi process unavailable"),
            "unexpected error message: {}",
            resp.data[0].errors[0].message
        );
    }

    #[tokio::test]
    async fn skills_list_defaults_to_current_dir_when_empty() {
        let state = dummy_state().await;
        let resp = handle_skills_list(&state, p::SkillsListParams::default()).await;
        assert_eq!(resp.data.len(), 1);
    }

    #[tokio::test]
    async fn skills_config_write_round_trips_enabled() {
        let state = dummy_state().await;
        let resp = handle_skills_config_write(
            &state,
            p::SkillsConfigWriteParams {
                path: Some("/repo/.pi/skills/rotate-keys".into()),
                name: None,
                enabled: false,
            },
        )
        .await;
        assert!(!resp.effective_enabled);
    }

    #[tokio::test]
    async fn skills_remote_list_returns_empty_data() {
        let state = dummy_state().await;
        let v = handle_skills_remote_list(&state).await;
        assert_eq!(v, json!({ "data": [] }));
    }
}
