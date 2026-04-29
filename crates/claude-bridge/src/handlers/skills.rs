//! `skills/list`, `skills/remote/list`, `skills/remote/export`,
//! `skills/config/write`.
//!
//! `skills/list` reads from the bridge cache the event pump refreshes on
//! every claude `system/init`. If no live process has emitted init yet the
//! handler returns an empty list — init populates lazily on the first real
//! `turn/start`, not on demand. (A utility-spawn with no user envelope sits
//! silent and `wait_for_init` would block until its 30s deadline; v2's
//! cleaner-init-path work in task #13 is what fixes the cold path properly.)
//!
//! Skill metadata claude exposes through `system/init.skills` is a flat list
//! of names — claude does not surface descriptions, paths, or icons over the
//! wire today. The bridge synthesizes a minimal `SkillMetadata` per name with
//! `scope: User` (claude skills are user-installed under `~/.claude/skills/`).
//!
//! Remote skills and skill config write are out of scope for v1; both are
//! empty-success stubs.

use std::path::PathBuf;
use std::sync::Arc;

use alleycat_codex_proto as p;
use serde_json::Value;
use serde_json::json;

use crate::state::ConnectionState;

pub async fn handle_skills_list(
    state: &Arc<ConnectionState>,
    params: p::SkillsListParams,
) -> p::SkillsListResponse {
    let cwds = if params.cwds.is_empty() {
        vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))]
    } else {
        params.cwds
    };

    let skills: Vec<p::SkillMetadata> = state
        .caches()
        .skills
        .into_iter()
        .map(synthesize_skill_metadata)
        .collect();

    // Claude's skills view is process-wide, not per-cwd. Mirror the same
    // metadata under each requested cwd so codex clients see a consistent
    // answer regardless of which cwd they probed.
    let data = cwds
        .into_iter()
        .map(|cwd| p::SkillsListEntry {
            cwd,
            skills: skills.clone(),
            errors: Vec::new(),
        })
        .collect();

    p::SkillsListResponse { data }
}

pub async fn handle_skills_remote_list(_state: &Arc<ConnectionState>) -> Value {
    json!({ "data": [] })
}

pub async fn handle_skills_remote_export(_state: &Arc<ConnectionState>, _params: Value) -> Value {
    json!({})
}

pub async fn handle_skills_config_write(
    _state: &Arc<ConnectionState>,
    params: p::SkillsConfigWriteParams,
) -> p::SkillsConfigWriteResponse {
    // Round-trip the requested `enabled` flag so the codex UI doesn't error
    // when toggling a skill — the bridge does not persist this preference.
    p::SkillsConfigWriteResponse {
        effective_enabled: params.enabled,
    }
}

/// Build a minimal `SkillMetadata` from just a name. Claude does not expose
/// description/path/scope over the wire today, so we report scope=User
/// (matches `~/.claude/skills/<name>/`) and leave fields the bridge cannot
/// know empty.
fn synthesize_skill_metadata(name: String) -> p::SkillMetadata {
    p::SkillMetadata {
        name,
        description: String::new(),
        short_description: None,
        interface: None,
        dependencies: None,
        path: String::new(),
        scope: p::SkillScope::User,
        enabled: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::ClaudeSessionRef;
    use crate::pool::ClaudePool;
    use crate::pool::claude_protocol::SystemInit;
    use crate::state::{ConnectionState, ThreadDefaults};
    use std::collections::BTreeMap;

    struct NoopIndex;

    #[async_trait::async_trait]
    impl alleycat_bridge_core::ThreadIndexHandle<ClaudeSessionRef> for NoopIndex {
        async fn lookup(&self, _: &str) -> Option<crate::state::IndexEntry> {
            None
        }
        async fn insert(&self, _: crate::state::IndexEntry) -> anyhow::Result<()> {
            Ok(())
        }
        async fn set_archived(&self, _: &str, _: bool) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn set_name(&self, _: &str, _: Option<String>) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn update_preview_and_updated_at(
            &self,
            _: &str,
            _: String,
            _: chrono::DateTime<chrono::Utc>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn list(
            &self,
            _: &crate::state::ListFilter,
            _: crate::state::ListSort,
            _: Option<&str>,
            _: Option<u32>,
        ) -> anyhow::Result<crate::state::ListPage<ClaudeSessionRef>> {
            Ok(crate::state::ListPage::<ClaudeSessionRef> {
                data: Vec::new(),
                next_cursor: None,
            })
        }
        async fn loaded_thread_ids(&self) -> Vec<String> {
            Vec::new()
        }
    }

    fn dummy_state() -> Arc<ConnectionState> {
        let (state, _rx) = ConnectionState::for_test(
            // `/usr/bin/false` exits immediately; utility spawn fails fast
            // so the test exercises the error fall-through.
            Arc::new(ClaudePool::new(PathBuf::from("/usr/bin/false"))),
            Arc::new(NoopIndex),
            ThreadDefaults::default(),
        );
        state
    }

    fn init_with_skills(names: &[&str]) -> SystemInit {
        SystemInit {
            session_id: "s".into(),
            cwd: "/x".into(),
            model: "claude-haiku".into(),
            tools: Vec::new(),
            mcp_servers: Vec::new(),
            slash_commands: Vec::new(),
            agents: Vec::new(),
            skills: names.iter().map(|s| s.to_string()).collect(),
            permission_mode: None,
            api_key_source: None,
            claude_code_version: None,
            output_style: None,
            uuid: None,
            extra: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn cached_skills_surface_per_cwd() {
        let state = dummy_state();
        state.refresh_init_cache(init_with_skills(&["debug", "simplify"]));

        let resp = handle_skills_list(
            &state,
            p::SkillsListParams {
                cwds: vec![PathBuf::from("/repo")],
                ..Default::default()
            },
        )
        .await;
        assert_eq!(resp.data.len(), 1);
        assert_eq!(resp.data[0].cwd, PathBuf::from("/repo"));
        assert_eq!(resp.data[0].skills.len(), 2);
        let names: Vec<_> = resp.data[0].skills.iter().map(|s| &s.name).collect();
        assert!(names.iter().any(|n| n.as_str() == "debug"));
        assert!(names.iter().any(|n| n.as_str() == "simplify"));
        assert!(matches!(resp.data[0].skills[0].scope, p::SkillScope::User));
        assert!(resp.data[0].errors.is_empty());
    }

    #[tokio::test]
    async fn empty_cache_yields_empty_skill_list_without_spawning() {
        // The handler must NOT touch the pool on a cold cache — a utility
        // claude spawn would block on `wait_for_init` (no user envelope =
        // claude stays silent) and deadlock the request for 30s. Instead it
        // returns the empty page; the next real `turn/start` will populate
        // the cache via the event pump.
        let state = dummy_state();
        let resp = handle_skills_list(
            &state,
            p::SkillsListParams {
                cwds: vec![PathBuf::from("/x")],
                ..Default::default()
            },
        )
        .await;
        assert_eq!(resp.data.len(), 1);
        assert!(resp.data[0].skills.is_empty());
        assert!(resp.data[0].errors.is_empty());
    }

    #[tokio::test]
    async fn config_write_round_trips_enabled_flag() {
        let state = dummy_state();
        let resp = handle_skills_config_write(
            &state,
            p::SkillsConfigWriteParams {
                path: Some("/x".into()),
                name: None,
                enabled: false,
            },
        )
        .await;
        assert!(!resp.effective_enabled);
    }

    #[tokio::test]
    async fn remote_endpoints_return_empty_envelopes() {
        let state = dummy_state();
        assert_eq!(
            handle_skills_remote_list(&state).await,
            json!({ "data": [] })
        );
        assert_eq!(
            handle_skills_remote_export(&state, json!({})).await,
            json!({})
        );
    }
}
