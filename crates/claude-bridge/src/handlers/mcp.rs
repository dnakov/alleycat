//! `mcpServerStatus/list`, `config/mcpServer/reload`,
//! `mcpServer/oauth/login`.
//!
//! `mcpServerStatus/list` reads from the bridge cache the event pump
//! refreshes on every claude `system/init` line. Until any thread starts the
//! cache is empty and the response is the empty list — codex clients accept
//! that as "no MCP servers registered yet".
//!
//! Refresh and oauth login are stubs in v1: claude reloads its own MCP config
//! on the next process spawn (we cannot ask a running `claude -p` to reload),
//! and OAuth flows for MCP servers run inside claude itself outside the
//! bridge's control. Both return empty success so the codex test client never
//! sees `method_not_found`.

use std::sync::Arc;

use alleycat_codex_proto as p;

use crate::state::ConnectionState;

pub fn handle_mcp_server_status_list(
    state: &Arc<ConnectionState>,
    _params: p::ListMcpServerStatusParams,
) -> p::ListMcpServerStatusResponse {
    let cached = state.caches().mcp_servers;
    let data = cached
        .into_iter()
        .map(|m| p::McpServerStatus {
            name: m.name,
            tools: serde_json::Value::Object(Default::default()),
            resources: Vec::new(),
            resource_templates: Vec::new(),
            auth_status: serde_json::Value::String(m.status),
        })
        .collect();
    p::ListMcpServerStatusResponse {
        data,
        next_cursor: None,
    }
}

pub fn handle_mcp_server_refresh(_state: &Arc<ConnectionState>) -> p::McpServerRefreshResponse {
    p::McpServerRefreshResponse::default()
}

pub fn handle_mcp_server_oauth_login(
    _state: &Arc<ConnectionState>,
    _params: p::McpServerOauthLoginParams,
) -> p::McpServerOauthLoginResponse {
    // Empty URL signals "no flow started". The matching
    // `mcpServer/oauthLogin/completed` notification is what would carry
    // success/failure — we never emit it.
    p::McpServerOauthLoginResponse {
        authorization_url: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::ClaudeSessionRef;
    use crate::pool::ClaudePool;
    use crate::pool::claude_protocol::{McpServerInit, SystemInit};
    use crate::state::{ConnectionState, ThreadDefaults};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

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
            Arc::new(ClaudePool::new(PathBuf::from("/usr/bin/false"))),
            Arc::new(NoopIndex),
            ThreadDefaults::default(),
        );
        state
    }

    #[test]
    fn empty_cache_yields_empty_list() {
        let state = dummy_state();
        let resp = handle_mcp_server_status_list(&state, p::ListMcpServerStatusParams::default());
        assert!(resp.data.is_empty());
        assert!(resp.next_cursor.is_none());
    }

    #[test]
    fn cached_init_servers_surface_with_status() {
        let state = dummy_state();
        let init = SystemInit {
            session_id: "s".into(),
            cwd: "/x".into(),
            model: "claude-haiku".into(),
            tools: Vec::new(),
            mcp_servers: vec![McpServerInit {
                name: "drive".into(),
                status: "needs-auth".into(),
                extra: BTreeMap::new(),
            }],
            slash_commands: Vec::new(),
            agents: Vec::new(),
            skills: Vec::new(),
            permission_mode: None,
            api_key_source: None,
            claude_code_version: None,
            output_style: None,
            uuid: None,
            extra: BTreeMap::new(),
        };
        state.refresh_init_cache(init);
        let resp = handle_mcp_server_status_list(&state, p::ListMcpServerStatusParams::default());
        assert_eq!(resp.data.len(), 1);
        assert_eq!(resp.data[0].name, "drive");
        assert_eq!(
            resp.data[0].auth_status,
            serde_json::Value::String("needs-auth".into())
        );
    }
}
