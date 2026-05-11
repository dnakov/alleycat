//! Hermes Bridge — Bridge trait implementation.
//!
//! Translates between the codex app-server JSON-RPC surface and the
//! Hermes Agent backend (API or CLI mode).

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use alleycat_bridge_core::{Bridge, Conn, JsonRpcError};
use alleycat_codex_proto::common::{
    ApprovalsReviewer, AskForApproval, ReasoningEffort, SessionSource, ThreadStatus, TurnStatus,
};
use alleycat_codex_proto::items::{ThreadItem, UserInput};
use alleycat_codex_proto::lifecycle::InitializeResponse;
use alleycat_codex_proto::model::ModelListResponse;
use alleycat_codex_proto::notifications::{
    AgentMessageDeltaNotification, ItemCompletedNotification, ItemStartedNotification,
    ThreadStartedNotification, TurnCompletedNotification, TurnStartedNotification,
};
use alleycat_codex_proto::thread::{
    Thread, ThreadForkParams, ThreadListParams, ThreadListResponse, ThreadResumeParams,
    ThreadStartParams, ThreadStartResponse, Turn,
};
use alleycat_codex_proto::turn::{TurnStartParams, TurnStartResponse};

use crate::api_client::{CreateRunRequest, DEFAULT_API_KEY_ENV, HermesApiClient};
use crate::config::HermesBridgeConfig;
use crate::index::{HermesBinding, ThreadIndex};
use crate::state::{ActiveTurn, TurnState};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn random_hex(len: usize) -> String {
    use rand::RngCore;
    let mut rng = rand::thread_rng();
    std::iter::repeat_with(|| rng.next_u32() as u8)
        .take(len)
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn rpc_error(code: i64, message: impl Into<String>) -> JsonRpcError {
    JsonRpcError {
        code,
        message: message.into(),
        data: None,
    }
}

fn error_response(code: i64, message: &str) -> Result<Value, JsonRpcError> {
    Err(rpc_error(code, message))
}

fn completed_turn(turn_id: &str) -> Turn {
    Turn {
        id: turn_id.to_string(),
        items: vec![],
        items_view: "full".to_string(),
        status: TurnStatus::Completed,
        error: None,
        started_at: Some(epoch_ms()),
        completed_at: Some(epoch_ms()),
        duration_ms: None,
    }
}

// ---------------------------------------------------------------------------
// HermesBridge
// ---------------------------------------------------------------------------

pub struct HermesBridge {
    config: HermesBridgeConfig,
    index: Arc<ThreadIndex>,
    state: Arc<TurnState>,
    api_client: Arc<HermesApiClient>,
}

impl HermesBridge {
    pub fn new(config: HermesBridgeConfig) -> Self {
        let api_base = match &config.mode {
            crate::config::HermesMode::Api { api_base } => api_base.clone(),
            crate::config::HermesMode::Auto { api_base, .. } => api_base.clone(),
            crate::config::HermesMode::Cli { .. } => "http://localhost:8321".to_string(),
        };
        let api_key = std::env::var("API_SERVER_KEY")
            .or_else(|_| std::env::var(DEFAULT_API_KEY_ENV))
            .ok();
        Self {
            config,
            index: Arc::new(ThreadIndex::new_in_memory()),
            state: Arc::new(TurnState::new()),
            api_client: Arc::new(HermesApiClient::new(&api_base, api_key)),
        }
    }

    fn make_thread(thread_id: &str, session_id: &str) -> Thread {
        let now = epoch_ms();
        Thread {
            id: thread_id.to_string(),
            session_id: session_id.to_string(),
            forked_from_id: None,
            preview: String::new(),
            ephemeral: true,
            model_provider: "hermes-agent".to_string(),
            created_at: now,
            updated_at: now,
            status: ThreadStatus::Idle,
            path: None,
            cwd: "/tmp".to_string(),
            cli_version: env!("CARGO_PKG_VERSION").to_string(),
            source: SessionSource::AppServer,
            thread_source: None,
            agent_nickname: Some("hermes".to_string()),
            agent_role: Some("assistant".to_string()),
            git_info: None,
            name: None,
            turns: vec![],
        }
    }
}

// ---------------------------------------------------------------------------
// Bridge trait
// ---------------------------------------------------------------------------

#[async_trait]
impl Bridge for HermesBridge {
    async fn initialize(&self, ctx: &Conn, params: Value) -> Result<Value, JsonRpcError> {
        ctx.set_initialize_capabilities(&params);

        let home = directories::ProjectDirs::from("", "", "alleycat")
            .map(|d| d.data_dir().to_string_lossy().to_string())
            .unwrap_or_else(|| "/tmp/alleycat".to_string());

        let resp = InitializeResponse {
            user_agent: format!("hermes-bridge/{}", env!("CARGO_PKG_VERSION")),
            codex_home: home,
            platform_family: "linux".to_string(),
            platform_os: std::env::consts::OS.to_string(),
        };
        Ok(serde_json::to_value(resp).unwrap_or_default())
    }

    async fn dispatch(
        &self,
        ctx: &Conn,
        method: &str,
        params: Value,
    ) -> Result<Value, JsonRpcError> {
        match method {
            // ---- Thread lifecycle ----
            "thread/start" => self.handle_thread_start(ctx, params).await,
            "thread/resume" => self.handle_thread_resume(ctx, params).await,
            "thread/fork" => self.handle_thread_fork(ctx, params).await,
            "thread/archive" => self.handle_thread_archive(ctx, params).await,
            "thread/unarchive" => self.handle_thread_unarchive(ctx, params).await,
            "thread/name/set" => self.handle_thread_name_set(ctx, params).await,
            "thread/compact/start" => self.handle_thread_compact_start(ctx, params).await,
            "thread/rollback" => self.handle_thread_rollback(ctx, params).await,
            "thread/list" => self.handle_thread_list(ctx, params).await,
            "thread/loaded/list" => self.handle_thread_loaded_list(ctx, params).await,
            "thread/read" => self.handle_thread_read(ctx, params).await,
            "thread/turns/list" => self.handle_thread_turns_list(ctx, params).await,
            "thread/backgroundTerminals/clean" => {
                self.handle_thread_background_terminals_clean(ctx, params)
                    .await
            }

            // ---- Turn lifecycle ----
            "turn/start" => self.handle_turn_start(ctx, params).await,
            "turn/steer" => self.handle_turn_steer(ctx, params).await,
            "turn/interrupt" => self.handle_turn_interrupt(ctx, params).await,

            // ---- Review ----
            "review/start" => self.handle_review_start(ctx, params).await,

            // ---- Models ----
            "model/list" => self.handle_model_list(ctx, params).await,

            // ---- Account ----
            "account/read" => self.handle_account_read(ctx, params).await,
            "account/rateLimits/read" => self.handle_account_rate_limits_read(ctx, params).await,
            "account/login/start" => self.handle_account_login_start(ctx, params).await,
            "account/login/cancel" => self.handle_account_login_cancel(ctx, params).await,
            "account/logout" => self.handle_account_logout(ctx, params).await,

            // ---- Config ----
            "config/read" => self.handle_config_read(ctx, params).await,
            "config/value/write" => self.handle_config_value_write(ctx, params).await,
            "config/batchWrite" => self.handle_config_batch_write(ctx, params).await,
            "configRequirements/read" => self.handle_config_requirements_read(ctx, params).await,

            // ---- MCP ----
            "mcpServerStatus/list" => self.handle_mcp_server_status_list(ctx, params).await,
            "config/mcpServer/reload" => self.handle_config_mcp_server_reload(ctx, params).await,
            "mcpServer/oauth/login" => self.handle_mcp_server_oauth_login(ctx, params).await,

            // ---- Skills ----
            "skills/list" => self.handle_skills_list(ctx, params).await,
            "skills/remote/list" => self.handle_skills_remote_list(ctx, params).await,
            "skills/remote/export" => self.handle_skills_remote_export(ctx, params).await,
            "skills/config/write" => self.handle_skills_config_write(ctx, params).await,

            // ---- Command ----
            "command/exec" => self.handle_command_exec(ctx, params).await,
            "command/exec/write" => self.handle_command_exec_write(ctx, params).await,
            "command/exec/terminate" => self.handle_command_exec_terminate(ctx, params).await,
            "command/exec/resize" => self.handle_command_exec_resize(ctx, params).await,

            // ---- Experimental ----
            "mock/experimentalMethod" => self.handle_mock_experimental_method(ctx, params).await,
            "experimentalFeature/list" => self.handle_experimental_feature_list(ctx, params).await,
            "collaborationMode/list" => self.handle_collaboration_mode_list(ctx, params).await,

            // ---- Feedback ----
            "feedback/upload" => self.handle_feedback_upload(ctx, params).await,

            _ => error_response(-32601, &format!("Method not found: {method}")),
        }
    }

    async fn notification(&self, _ctx: &Conn, _method: &str, _params: Value) {
        // Hermes bridge does not server-initiate notifications on its own.
    }
}

// ---------------------------------------------------------------------------
// Thread handlers
// ---------------------------------------------------------------------------

impl HermesBridge {
    async fn handle_thread_start(&self, ctx: &Conn, params: Value) -> Result<Value, JsonRpcError> {
        let p: ThreadStartParams = serde_json::from_value(params.clone())
            .map_err(|e| rpc_error(-32602, format!("Invalid params: {e}")))?;

        let thread_id = format!("thread_{}", random_hex(12));
        let session_id = format!("ses_{}", random_hex(12));

        let thread = Self::make_thread(&thread_id, &session_id);

        self.index.upsert(HermesBinding {
            thread_id: thread_id.clone(),
            hermes_session_id: session_id.clone(),
            model: p.model.clone(),
            created_at: epoch_ms(),
            preview: None,
        });

        // Notify client that a thread started.
        let _ = ctx.notifier().send_notification(
            "thread/started",
            ThreadStartedNotification {
                thread: thread.clone(),
            },
        );

        let resp = ThreadStartResponse {
            thread,
            model: "hermes-agent".to_string(),
            model_provider: "hermes-agent".to_string(),
            service_tier: None,
            cwd: "/tmp".to_string(),
            instruction_sources: vec![],
            approval_policy: AskForApproval::Never,
            approvals_reviewer: ApprovalsReviewer::User,
            sandbox: json!({"type": "danger-full-access"}),
            permission_profile: None,
            active_permission_profile: None,
            reasoning_effort: Some(ReasoningEffort::Medium),
        };
        Ok(serde_json::to_value(resp).unwrap_or_default())
    }

    async fn handle_thread_resume(
        &self,
        _ctx: &Conn,
        params: Value,
    ) -> Result<Value, JsonRpcError> {
        let _p: ThreadResumeParams = serde_json::from_value(params)
            .map_err(|e| rpc_error(-32602, format!("Invalid params: {e}")))?;
        // TODO: reconnect to existing Hermes session
        error_response(-32603, "thread/resume not yet implemented")
    }

    async fn handle_thread_fork(&self, _ctx: &Conn, params: Value) -> Result<Value, JsonRpcError> {
        let _p: ThreadForkParams = serde_json::from_value(params)
            .map_err(|e| rpc_error(-32602, format!("Invalid params: {e}")))?;
        error_response(-32603, "thread/fork not yet implemented")
    }

    async fn handle_thread_archive(
        &self,
        _ctx: &Conn,
        params: Value,
    ) -> Result<Value, JsonRpcError> {
        let tid = params
            .get("threadId")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if let Some(b) = self.index.remove(tid) {
            self.index.persist().ok();
            let thread = Self::make_thread(&b.thread_id, &b.hermes_session_id);
            let thread = Thread {
                status: ThreadStatus::Idle,
                ..thread
            };
            Ok(json!({ "thread": thread }))
        } else {
            error_response(-32602, "thread not found")
        }
    }

    async fn handle_thread_unarchive(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({}))
    }

    async fn handle_thread_name_set(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({}))
    }

    async fn handle_thread_compact_start(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({}))
    }

    async fn handle_thread_rollback(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({}))
    }

    async fn handle_thread_list(&self, _ctx: &Conn, params: Value) -> Result<Value, JsonRpcError> {
        let _p: ThreadListParams = serde_json::from_value(params)
            .map_err(|e| rpc_error(-32602, format!("Invalid params: {e}")))?;
        let ids = self.index.thread_ids();
        let threads: Vec<Thread> = ids
            .iter()
            .map(|id| Self::make_thread(id, "ses_unknown"))
            .collect();
        Ok(serde_json::to_value(ThreadListResponse {
            data: threads,
            next_cursor: None,
            backwards_cursor: None,
        })
        .unwrap_or_default())
    }

    async fn handle_thread_loaded_list(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        let ids = self.index.thread_ids();
        Ok(json!({ "threadIds": ids }))
    }

    async fn handle_thread_read(&self, _ctx: &Conn, params: Value) -> Result<Value, JsonRpcError> {
        let tid = params
            .get("threadId")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        match self.index.get_by_thread(tid) {
            Some(b) => {
                let thread = Self::make_thread(&b.thread_id, &b.hermes_session_id);
                Ok(json!({ "thread": thread }))
            }
            None => error_response(-32602, "thread not found"),
        }
    }

    async fn handle_thread_turns_list(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({ "turns": [] }))
    }

    async fn handle_thread_background_terminals_clean(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({}))
    }
}

// ---------------------------------------------------------------------------
// Turn handlers
// ---------------------------------------------------------------------------

impl HermesBridge {
    async fn handle_turn_start(&self, ctx: &Conn, params: Value) -> Result<Value, JsonRpcError> {
        let p: TurnStartParams = serde_json::from_value(params)
            .map_err(|e| rpc_error(-32602, format!("Invalid params: {e}")))?;

        let thread_id = p.thread_id.clone();
        let turn_id = format!("turn_{}", random_hex(12));

        // Resolve session from index.
        let binding = self
            .index
            .get_by_thread(&thread_id)
            .ok_or_else(|| rpc_error(-32602, "thread not found"))?;
        let session_id = binding.hermes_session_id.clone();

        // Track the turn.
        self.state.insert(
            thread_id.clone(),
            ActiveTurn {
                turn_id: turn_id.clone(),
                thread_id: thread_id.clone(),
                hermes_session_id: session_id.clone(),
                run_id: None,
            },
        );

        // Notify turn started.
        let turn = Turn {
            id: turn_id.clone(),
            items: vec![],
            items_view: "full".to_string(),
            status: TurnStatus::InProgress,
            error: None,
            started_at: Some(epoch_ms()),
            completed_at: None,
            duration_ms: None,
        };
        let _ = ctx.notifier().send_notification(
            "turn/started",
            TurnStartedNotification {
                thread_id: thread_id.clone(),
                turn: turn.clone(),
            },
        );

        // Emit the user message as an item.
        let user_item_id = format!("item_{}", random_hex(8));
        let _ = ctx.notifier().send_notification(
            "item/started",
            ItemStartedNotification {
                item: ThreadItem::UserMessage {
                    id: user_item_id.clone(),
                    content: p.input.clone(),
                },
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
                parent_item_id: None,
            },
        );
        let _ = ctx.notifier().send_notification(
            "item/completed",
            ItemCompletedNotification {
                item: ThreadItem::UserMessage {
                    id: user_item_id.clone(),
                    content: p.input.clone(),
                },
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
                parent_item_id: None,
            },
        );

        // Dispatch turn to Hermes backend.
        match &self.config.mode {
            crate::config::HermesMode::Api { .. } => {
                self.dispatch_turn_api(ctx, &thread_id, &turn_id, &session_id, &p)
                    .await
            }
            crate::config::HermesMode::Auto { .. } => {
                if self
                    .api_client
                    .health()
                    .await
                    .map(|h| h.status == "ok")
                    .unwrap_or(false)
                {
                    match self
                        .dispatch_turn_api(ctx, &thread_id, &turn_id, &session_id, &p)
                        .await
                    {
                        Ok(value) => Ok(value),
                        Err(_) => self.dispatch_turn_cli(ctx, &thread_id, &turn_id, &p).await,
                    }
                } else {
                    self.dispatch_turn_cli(ctx, &thread_id, &turn_id, &p).await
                }
            }
            crate::config::HermesMode::Cli { .. } => {
                self.dispatch_turn_cli(ctx, &thread_id, &turn_id, &p).await
            }
        }
    }

    async fn handle_turn_steer(&self, _ctx: &Conn, _params: Value) -> Result<Value, JsonRpcError> {
        // Steer is not supported — returns empty for now.
        Ok(json!({}))
    }

    async fn handle_turn_interrupt(
        &self,
        _ctx: &Conn,
        params: Value,
    ) -> Result<Value, JsonRpcError> {
        let thread_id = params
            .get("threadId")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if let Some(active) = self.state.remove(thread_id)
            && let Some(run_id) = active.run_id
        {
            let _ = self.api_client.stop_run(&run_id).await;
        }
        Ok(json!({}))
    }

    async fn handle_review_start(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        error_response(-32601, "review/start not supported")
    }
}

// ---------------------------------------------------------------------------
// Model / Account / Config stubs
// ---------------------------------------------------------------------------

impl HermesBridge {
    async fn handle_model_list(&self, _ctx: &Conn, _params: Value) -> Result<Value, JsonRpcError> {
        let resp = ModelListResponse {
            data: vec![alleycat_codex_proto::model::Model {
                id: "hermes-agent".to_string(),
                model: "hermes-agent".to_string(),
                upgrade: None,
                upgrade_info: None,
                availability_nux: None,
                display_name: "Hermes Agent".to_string(),
                description: "Hermes Agent via Alleycat bridge".to_string(),
                hidden: false,
                supported_reasoning_efforts: vec![
                    alleycat_codex_proto::model::ReasoningEffortOption {
                        reasoning_effort: ReasoningEffort::Medium,
                        description: "Default Hermes reasoning effort".to_string(),
                    },
                ],
                default_reasoning_effort: ReasoningEffort::Medium,
                input_modalities: vec![json!("text")],
                supports_personality: false,
                additional_speed_tiers: vec![],
                service_tiers: vec![],
                is_default: true,
            }],
            next_cursor: None,
        };
        Ok(serde_json::to_value(resp).unwrap_or_default())
    }

    async fn handle_account_read(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({ "account": null, "requiresOpenaiAuth": false }))
    }

    async fn handle_account_rate_limits_read(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({ "rateLimits": {}, "rateLimitsByLimitId": null }))
    }

    async fn handle_account_login_start(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({}))
    }

    async fn handle_account_login_cancel(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({}))
    }

    async fn handle_account_logout(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({}))
    }

    async fn handle_config_read(&self, _ctx: &Conn, _params: Value) -> Result<Value, JsonRpcError> {
        Ok(json!({}))
    }

    async fn handle_config_value_write(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({}))
    }

    async fn handle_config_batch_write(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({}))
    }

    async fn handle_config_requirements_read(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({}))
    }

    async fn handle_mcp_server_status_list(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({ "servers": [] }))
    }

    async fn handle_config_mcp_server_reload(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({}))
    }

    async fn handle_mcp_server_oauth_login(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({}))
    }

    async fn handle_skills_list(&self, _ctx: &Conn, _params: Value) -> Result<Value, JsonRpcError> {
        Ok(json!({ "skills": [] }))
    }

    async fn handle_skills_remote_list(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({ "skills": [] }))
    }

    async fn handle_skills_remote_export(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({}))
    }

    async fn handle_skills_config_write(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({}))
    }

    async fn handle_command_exec(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        error_response(-32601, "command/exec not supported by hermes bridge")
    }

    async fn handle_command_exec_write(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        error_response(-32601, "command/exec/write not supported by hermes bridge")
    }

    async fn handle_command_exec_terminate(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        error_response(
            -32601,
            "command/exec/terminate not supported by hermes bridge",
        )
    }

    async fn handle_command_exec_resize(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        error_response(-32601, "command/exec/resize not supported by hermes bridge")
    }

    async fn handle_mock_experimental_method(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({}))
    }

    async fn handle_experimental_feature_list(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({ "features": [] }))
    }

    async fn handle_collaboration_mode_list(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({ "modes": [] }))
    }

    async fn handle_feedback_upload(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({}))
    }
}

// ---------------------------------------------------------------------------
// Turn dispatch — API mode
// ---------------------------------------------------------------------------

impl HermesBridge {
    async fn dispatch_turn_api(
        &self,
        ctx: &Conn,
        thread_id: &str,
        turn_id: &str,
        _session_id: &str,
        params: &TurnStartParams,
    ) -> Result<Value, JsonRpcError> {
        // Extract user text from TurnStartParams.input
        let user_text: String = params
            .input
            .iter()
            .filter_map(|inp| match inp {
                UserInput::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<&str>>()
            .join("\n");

        let request = CreateRunRequest {
            input: user_text,
            session_id: Some(_session_id.to_string()),
            cwd: params.cwd.as_ref().map(|p| p.to_string_lossy().to_string()),
            model: params.model.clone(),
        };
        match self.api_client.create_run(request).await {
            Ok(run) => {
                if let Some(ref hermes_session_id) = run.session_id {
                    self.index.upsert(HermesBinding {
                        thread_id: thread_id.to_string(),
                        hermes_session_id: hermes_session_id.clone(),
                        model: params.model.clone(),
                        created_at: epoch_ms(),
                        preview: None,
                    });
                }
                self.state.insert(
                    thread_id.to_string(),
                    ActiveTurn {
                        turn_id: turn_id.to_string(),
                        thread_id: thread_id.to_string(),
                        hermes_session_id: _session_id.to_string(),
                        run_id: Some(run.run_id.clone()),
                    },
                );
                let mut text = String::new();
                match self.api_client.events_stream(&run.run_id).await {
                    Ok(resp) => {
                        let mut body = String::new();
                        let mut stream = resp.bytes_stream();
                        while let Some(chunk) = stream.next().await {
                            match chunk {
                                Ok(bytes) => {
                                    body.push_str(&String::from_utf8_lossy(&bytes));
                                    let split_at = body.rfind(
                                        "

",
                                    );
                                    if let Some(idx) = split_at {
                                        let complete = body[..idx + 2].to_string();
                                        body = body[idx + 2..].to_string();
                                        for event in crate::sse::parse_sse_frames(&complete) {
                                            if let Some(delta) = event.message_delta() {
                                                text.push_str(&delta);
                                                self.emit_agent_delta(
                                                    ctx, thread_id, turn_id, &delta,
                                                )
                                                .await;
                                            } else if let Some(error) = event.terminal_error() {
                                                return error_response(
                                                    -32603,
                                                    &format!("Hermes API error: {error}"),
                                                );
                                            } else if event.is_terminal_success() {
                                                break;
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    return error_response(
                                        -32603,
                                        &format!("Hermes SSE error: {e}"),
                                    );
                                }
                            }
                        }
                        if !body.trim().is_empty() {
                            for event in crate::sse::parse_sse_frames(&body) {
                                if let Some(delta) = event.message_delta() {
                                    text.push_str(&delta);
                                    self.emit_agent_delta(ctx, thread_id, turn_id, &delta).await;
                                }
                            }
                        }
                    }
                    Err(e) => return error_response(-32603, &format!("Hermes events error: {e}")),
                }
                self.emit_turn_completed(ctx, thread_id, turn_id, Some(&text), None)
                    .await;
                let resp = TurnStartResponse {
                    turn: completed_turn(turn_id),
                };
                Ok(serde_json::to_value(resp).unwrap_or_default())
            }
            Err(e) => {
                self.emit_turn_completed(
                    ctx,
                    thread_id,
                    turn_id,
                    None,
                    Some(format!("Hermes API error: {e}")),
                )
                .await;
                error_response(-32603, &format!("Hermes API error: {e}"))
            }
        }
    }

    async fn dispatch_turn_cli(
        &self,
        ctx: &Conn,
        thread_id: &str,
        turn_id: &str,
        params: &TurnStartParams,
    ) -> Result<Value, JsonRpcError> {
        let user_text: String = params
            .input
            .iter()
            .filter_map(|inp| match inp {
                UserInput::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<&str>>()
            .join("\n");

        let (bin, session_id) = match &self.config.mode {
            crate::config::HermesMode::Cli { bin }
            | crate::config::HermesMode::Auto { bin, .. } => (
                bin.clone().unwrap_or_else(|| "hermes".to_string()),
                self.index
                    .get_by_thread(thread_id)
                    .map(|b| b.hermes_session_id),
            ),
            crate::config::HermesMode::Api { .. } => ("hermes".to_string(), None),
        };
        match crate::cli_adapter::run_hermes_cli(&bin, &user_text, session_id.as_deref(), None)
            .await
        {
            Ok(text) => {
                self.emit_synthetic_completion(ctx, thread_id, turn_id, &text)
                    .await;
                let resp = TurnStartResponse {
                    turn: completed_turn(turn_id),
                };
                Ok(serde_json::to_value(resp).unwrap_or_default())
            }
            Err(e) => error_response(-32603, &format!("Hermes CLI error: {e}")),
        }
    }

    async fn emit_agent_delta(&self, ctx: &Conn, thread_id: &str, turn_id: &str, text: &str) {
        let item_id = format!("item_{}", random_hex(8));
        let _ = ctx.notifier().send_notification(
            "item/started",
            ItemStartedNotification {
                item: ThreadItem::AgentMessage {
                    id: item_id.clone(),
                    text: String::new(),
                    phase: None,
                    memory_citation: None,
                },
                thread_id: thread_id.to_string(),
                turn_id: turn_id.to_string(),
                parent_item_id: None,
            },
        );
        let _ = ctx.notifier().send_notification(
            "item/agentMessage/delta",
            AgentMessageDeltaNotification {
                thread_id: thread_id.to_string(),
                turn_id: turn_id.to_string(),
                item_id: item_id.clone(),
                delta: text.to_string(),
                parent_item_id: None,
            },
        );
        let _ = ctx.notifier().send_notification(
            "item/completed",
            ItemCompletedNotification {
                item: ThreadItem::AgentMessage {
                    id: item_id,
                    text: text.to_string(),
                    phase: None,
                    memory_citation: None,
                },
                thread_id: thread_id.to_string(),
                turn_id: turn_id.to_string(),
                parent_item_id: None,
            },
        );
    }

    async fn emit_turn_completed(
        &self,
        ctx: &Conn,
        thread_id: &str,
        turn_id: &str,
        _text: Option<&str>,
        error: Option<String>,
    ) {
        let status = if error.is_some() {
            TurnStatus::Failed
        } else {
            TurnStatus::Completed
        };
        let turn = Turn {
            id: turn_id.to_string(),
            items: vec![],
            items_view: "full".to_string(),
            status,
            error: error.map(|message| alleycat_codex_proto::common::TurnError {
                message,
                codex_error_info: None,
                additional_details: None,
            }),
            started_at: Some(epoch_ms()),
            completed_at: Some(epoch_ms()),
            duration_ms: None,
        };
        let _ = ctx.notifier().send_notification(
            "turn/completed",
            TurnCompletedNotification {
                thread_id: thread_id.to_string(),
                turn,
            },
        );
        self.state.remove(thread_id);
    }

    /// Emit a synthetic agent-message + turn-completed sequence.
    async fn emit_synthetic_completion(
        &self,
        ctx: &Conn,
        thread_id: &str,
        turn_id: &str,
        text: &str,
    ) {
        let agent_item_id = format!("item_{}", random_hex(8));

        let _ = ctx.notifier().send_notification(
            "item/started",
            ItemStartedNotification {
                item: ThreadItem::AgentMessage {
                    id: agent_item_id.clone(),
                    text: String::new(),
                    phase: None,
                    memory_citation: None,
                },
                thread_id: thread_id.to_string(),
                turn_id: turn_id.to_string(),
                parent_item_id: None,
            },
        );

        let _ = ctx.notifier().send_notification(
            "item/agentMessage/delta",
            AgentMessageDeltaNotification {
                thread_id: thread_id.to_string(),
                turn_id: turn_id.to_string(),
                item_id: agent_item_id.clone(),
                delta: text.to_string(),
                parent_item_id: None,
            },
        );

        let _ = ctx.notifier().send_notification(
            "item/completed",
            ItemCompletedNotification {
                item: ThreadItem::AgentMessage {
                    id: agent_item_id.clone(),
                    text: text.to_string(),
                    phase: None,
                    memory_citation: None,
                },
                thread_id: thread_id.to_string(),
                turn_id: turn_id.to_string(),
                parent_item_id: None,
            },
        );

        // End the turn.
        let turn = Turn {
            id: turn_id.to_string(),
            items: vec![],
            items_view: "full".to_string(),
            status: TurnStatus::Completed,
            error: None,
            started_at: Some(epoch_ms()),
            completed_at: Some(epoch_ms()),
            duration_ms: None,
        };
        let _ = ctx.notifier().send_notification(
            "turn/completed",
            TurnCompletedNotification {
                thread_id: thread_id.to_string(),
                turn,
            },
        );

        self.state.remove(thread_id);
    }
}
