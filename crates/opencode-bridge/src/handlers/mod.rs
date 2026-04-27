use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use alleycat_bridge_core::{Bridge, Conn, JsonRpcError};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::index::{OpencodeBinding, ThreadIndex};
use crate::opencode_client::OpencodeClient;
use crate::opencode_proc::OpencodeRuntime;
use crate::pty::PtyState;
use crate::sse::SseConsumer;
use crate::state::{ActiveTurn, BridgeState};
use crate::translate::{input::codex_input_to_parts, parts::message_to_turn_items};

pub struct OpencodeBridge {
    client: OpencodeClient,
    index: Arc<ThreadIndex>,
    state: Arc<BridgeState>,
    pty: Arc<PtyState>,
    sse: SseConsumer,
}

impl OpencodeBridge {
    pub async fn new(runtime: OpencodeRuntime) -> anyhow::Result<Self> {
        let state_dir = std::env::var_os("ALLEYCAT_BRIDGE_STATE_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("alleycat-opencode-bridge"));
        let index = Arc::new(ThreadIndex::open(state_dir.join("threads.json")).await?);
        let client = OpencodeClient::new(runtime.base_url, runtime.auth_token);
        let sse = SseConsumer::spawn(client.clone());
        Ok(Self {
            client,
            index,
            state: Arc::new(BridgeState::default()),
            pty: Arc::new(PtyState::new()),
            sse,
        })
    }

    /// Subscribe a per-connection task that translates SSE events into codex
    /// notifications on `ctx`. The task ends when the connection closes (the
    /// notifier `send_notification` will start failing, but the broadcast
    /// receiver simply continues and any send failure is dropped).
    fn spawn_event_pump(&self, ctx: &Conn) {
        let mut rx = self.sse.subscribe();
        let index = Arc::clone(&self.index);
        let state = Arc::clone(&self.state);
        let pty = Arc::clone(&self.pty);
        let client = self.client.clone();
        let ctx = ctx.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let rc = crate::translate::events::RouteContext {
                            conn: &ctx,
                            index: &index,
                            state: &state,
                            client: &client,
                            pty_state: &pty,
                        };
                        crate::translate::events::route_event(rc, (*event).clone()).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(
                            "opencode SSE subscriber lagged, skipped {skipped} events"
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    async fn handle_thread_start(&self, params: Value) -> Result<Value, JsonRpcError> {
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            });
        let title = params
            .get("serviceName")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let permission = params.get("approvalPolicy").and_then(permission_from_codex);
        let session = self
            .client
            .create_session(title, permission)
            .await
            .map_err(|err| JsonRpcError::internal(format!("{err:#}")))?;
        let mut binding = self
            .index
            .bind_session(&session)
            .await
            .map_err(|err| JsonRpcError::internal(format!("{err:#}")))?;
        binding.directory = cwd.clone();
        self.index
            .insert(binding.clone())
            .await
            .map_err(|err| JsonRpcError::internal(format!("{err:#}")))?;
        let thread = binding_to_thread(&binding);
        Ok(json!({
            "thread": thread,
            "model": params.get("model").and_then(Value::as_str).unwrap_or("opencode"),
            "modelProvider": params.get("modelProvider").and_then(Value::as_str).unwrap_or("opencode"),
            "cwd": cwd,
            "instructionSources": [],
            "approvalPolicy": params.get("approvalPolicy").cloned().unwrap_or(json!("untrusted")),
            "approvalsReviewer": params.get("approvalsReviewer").cloned().unwrap_or(json!("user")),
            "sandbox": {"mode":"workspace-write"}
        }))
    }

    async fn handle_turn_start(&self, ctx: &Conn, params: Value) -> Result<Value, JsonRpcError> {
        let thread_id = params
            .get("threadId")
            .and_then(Value::as_str)
            .ok_or_else(|| JsonRpcError::invalid_params("threadId is required"))?;
        let binding = self
            .index
            .by_thread(thread_id)
            .ok_or_else(|| JsonRpcError::invalid_params(format!("unknown thread `{thread_id}`")))?;
        let input = params
            .get("input")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let parts = codex_input_to_parts(&input);
        let turn_id = format!("turn-{}", now_secs());
        self.state.set_active_turn(
            thread_id.to_string(),
            ActiveTurn {
                turn_id: turn_id.clone(),
                model: params
                    .get("model")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                session_id: Some(binding.session_id.clone()),
                current_assistant_message_id: None,
            },
        );
        let turn = json!({"id":turn_id,"items":[],"status":"inProgress","startedAt":now_secs()});
        let _ = ctx.notifier().send_notification(
            "turn/started",
            json!({"threadId":thread_id,"turn":turn.clone()}),
        );
        let model = params.get("model").and_then(Value::as_str).map(split_model);
        let mut body = json!({ "parts": parts });
        if let Some((provider_id, model_id)) = model {
            body["model"] = json!({"providerID":provider_id,"modelID":model_id});
        }
        // Async dispatch: opencode acks the prompt with 204 immediately and
        // streams every subsequent item over SSE. The bridge replies to codex
        // here with `status:"inProgress"` and an empty item list — the SSE
        // pump is responsible for `item/started`, deltas, `item/completed`,
        // and the final `turn/completed` (gated on `session.idle`).
        self.client
            .prompt_async(&binding.session_id, body)
            .await
            .map_err(|err| JsonRpcError::internal(format!("{err:#}")))?;
        Ok(json!({ "turn": turn }))
    }

    async fn handle_thread_compact_start(&self, params: Value) -> Result<Value, JsonRpcError> {
        let binding = binding_from_params(&self.index, &params)?;
        let (provider_id, model_id) = self
            .resolve_model(params.get("model").and_then(Value::as_str))
            .await?;
        self.client
            .summarize_session(&binding.session_id, &provider_id, &model_id)
            .await
            .map_err(|err| JsonRpcError::internal(format!("{err:#}")))?;
        Ok(json!({}))
    }

    async fn handle_thread_rollback(&self, params: Value) -> Result<Value, JsonRpcError> {
        let num_turns = params
            .get("numTurns")
            .or_else(|| params.get("num_turns"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if num_turns == 0 {
            return Err(JsonRpcError::invalid_params("numTurns must be >= 1"));
        }
        let binding = binding_from_params(&self.index, &params)?;
        let messages = self
            .client
            .list_messages(&binding.session_id)
            .await
            .map_err(|err| JsonRpcError::internal(format!("{err:#}")))?;
        let user_ids: Vec<String> = messages
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|message| {
                let role = message.pointer("/info/role").and_then(Value::as_str)?;
                if role != "user" {
                    return None;
                }
                message
                    .pointer("/info/id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .collect();
        let target_index = user_ids
            .len()
            .checked_sub(num_turns as usize)
            .ok_or_else(|| {
                JsonRpcError::invalid_params(format!(
                    "thread has {} user messages; cannot rollback {num_turns}",
                    user_ids.len()
                ))
            })?;
        let target_id = user_ids
            .get(target_index)
            .ok_or_else(|| JsonRpcError::internal("rollback anchor out of range"))?;
        self.client
            .revert_session(&binding.session_id, target_id)
            .await
            .map_err(|err| JsonRpcError::internal(format!("{err:#}")))?;
        let messages_after = self
            .client
            .list_messages(&binding.session_id)
            .await
            .unwrap_or(Value::Array(Vec::new()));
        let turns = messages_after
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|message| {
                let id = message
                    .pointer("/info/id")
                    .and_then(Value::as_str)
                    .unwrap_or("message")
                    .to_string();
                json!({"id":id,"items":message_to_turn_items(&message),"status":"completed"})
            })
            .collect::<Vec<_>>();
        let mut thread = binding_to_thread(&binding);
        thread["turns"] = json!(turns);
        Ok(json!({ "thread": thread }))
    }

    async fn handle_thread_fork(&self, params: Value) -> Result<Value, JsonRpcError> {
        let binding = binding_from_params(&self.index, &params)?;
        let messages = self
            .client
            .list_messages(&binding.session_id)
            .await
            .map_err(|err| JsonRpcError::internal(format!("{err:#}")))?;
        let leaf_user_id = messages
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .rev()
            .find_map(|message| {
                let role = message.pointer("/info/role").and_then(Value::as_str)?;
                if role != "user" {
                    return None;
                }
                message
                    .pointer("/info/id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            });
        let new_session = self
            .client
            .fork_session(&binding.session_id, leaf_user_id.as_deref())
            .await
            .map_err(|err| JsonRpcError::internal(format!("{err:#}")))?;
        let mut new_binding = self
            .index
            .bind_session(&new_session)
            .await
            .map_err(|err| JsonRpcError::internal(format!("{err:#}")))?;
        if new_binding.directory.is_empty() {
            new_binding.directory = binding.directory.clone();
            self.index
                .insert(new_binding.clone())
                .await
                .map_err(|err| JsonRpcError::internal(format!("{err:#}")))?;
        }
        let mut thread = binding_to_thread(&new_binding);
        thread["forkedFromId"] = json!(binding.thread_id);
        let model = params
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("opencode")
            .to_string();
        let model_provider = params
            .get("modelProvider")
            .and_then(Value::as_str)
            .unwrap_or("opencode")
            .to_string();
        Ok(json!({
            "thread": thread,
            "model": model,
            "modelProvider": model_provider,
            "cwd": new_binding.directory,
            "instructionSources": [],
            "approvalPolicy": params.get("approvalPolicy").cloned().unwrap_or(json!("untrusted")),
            "approvalsReviewer": params.get("approvalsReviewer").cloned().unwrap_or(json!("user")),
            "sandbox": {"mode":"workspace-write"},
            "permissionProfile": params.get("permissionProfile").cloned().unwrap_or(Value::Null),
            "reasoningEffort": params.get("reasoningEffort").cloned().unwrap_or(Value::Null),
            "serviceTier": Value::Null
        }))
    }

    async fn resolve_model(
        &self,
        explicit: Option<&str>,
    ) -> Result<(String, String), JsonRpcError> {
        if let Some(model) = explicit {
            let (provider_id, model_id) = split_model(model);
            return Ok((provider_id.to_string(), model_id.to_string()));
        }
        let providers = self
            .client
            .get("/config/providers")
            .await
            .map_err(|err| JsonRpcError::internal(format!("{err:#}")))?;
        providers
            .get("default")
            .and_then(Value::as_object)
            .and_then(|map| map.iter().next())
            .and_then(|(provider_id, model_id)| {
                model_id
                    .as_str()
                    .map(|id| (provider_id.clone(), id.to_string()))
            })
            .ok_or_else(|| {
                JsonRpcError::invalid_params(
                    "no model specified and no default provider/model configured",
                )
            })
    }

    async fn handle_thread_list(&self, params: Value) -> Result<Value, JsonRpcError> {
        let mut path = "/session".to_string();
        let mut query = Vec::new();
        if let Some(cwd) = params.get("cwd").and_then(Value::as_str) {
            query.push(format!("directory={}", encode_query(cwd)));
        }
        if let Some(search) = params.get("searchTerm").and_then(Value::as_str) {
            query.push(format!("search={}", encode_query(search)));
        }
        if !query.is_empty() {
            path.push('?');
            path.push_str(&query.join("&"));
        }
        let sessions = self
            .client
            .get(&path)
            .await
            .map_err(|err| JsonRpcError::internal(format!("{err:#}")))?;
        let array = sessions.as_array().cloned().unwrap_or_default();
        let mut data = Vec::new();
        for session in array {
            let binding = self
                .index
                .bind_session(&session)
                .await
                .map_err(|err| JsonRpcError::internal(format!("{err:#}")))?;
            data.push(binding_to_thread(&binding));
        }
        Ok(json!({"data":data,"nextCursor":null}))
    }

    async fn handle_thread_resume_or_read(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, JsonRpcError> {
        let thread_id = params
            .get("threadId")
            .or_else(|| params.get("thread_id"))
            .and_then(Value::as_str)
            .ok_or_else(|| JsonRpcError::invalid_params("threadId is required"))?;
        let binding = self
            .index
            .by_thread(thread_id)
            .ok_or_else(|| JsonRpcError::invalid_params("unknown thread"))?;
        let messages = self
            .client
            .get(&format!("/session/{}/message", binding.session_id))
            .await
            .map_err(|err| JsonRpcError::internal(format!("{err:#}")))?;
        let turns = messages
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|message| json!({"id":message.pointer("/info/id").and_then(Value::as_str).unwrap_or("message"),"items":message_to_turn_items(&message),"status":"completed"}))
            .collect::<Vec<_>>();
        let mut thread = binding_to_thread(&binding);
        thread["turns"] = json!(turns);
        if method == "thread/read" {
            Ok(json!({"thread":thread}))
        } else {
            Ok(
                json!({"thread":thread,"model":"opencode","modelProvider":"opencode","cwd":binding.directory}),
            )
        }
    }

    async fn handle_thread_archive(&self, params: Value) -> Result<Value, JsonRpcError> {
        let binding = binding_from_params(&self.index, &params)?;
        self.client
            .patch(
                &format!("/session/{}", binding.session_id),
                json!({"time":{"archived":now_secs() * 1000}}),
            )
            .await
            .map_err(|err| JsonRpcError::internal(format!("{err:#}")))?;
        Ok(json!({}))
    }

    async fn handle_thread_unarchive(&self, params: Value) -> Result<Value, JsonRpcError> {
        let binding = binding_from_params(&self.index, &params)?;
        self.client
            .patch(
                &format!("/session/{}", binding.session_id),
                json!({"time":{"archived":null}}),
            )
            .await
            .map_err(|err| JsonRpcError::internal(format!("{err:#}")))?;
        Ok(json!({"thread":binding_to_thread(&binding)}))
    }

    async fn handle_thread_name_set(&self, params: Value) -> Result<Value, JsonRpcError> {
        let binding = binding_from_params(&self.index, &params)?;
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        self.client
            .patch(
                &format!("/session/{}", binding.session_id),
                json!({"title":name}),
            )
            .await
            .map_err(|err| JsonRpcError::internal(format!("{err:#}")))?;
        Ok(json!({}))
    }

    async fn handle_turn_interrupt(&self, params: Value) -> Result<Value, JsonRpcError> {
        let binding = binding_from_params(&self.index, &params)?;
        self.client
            .post(&format!("/session/{}/abort", binding.session_id), json!({}))
            .await
            .map_err(|err| JsonRpcError::internal(format!("{err:#}")))?;
        Ok(json!({}))
    }

    async fn handle_model_list(&self) -> Result<Value, JsonRpcError> {
        let providers = self.client.get("/provider").await.unwrap_or(json!({}));
        Ok(json!({"data":flatten_models(providers),"nextCursor":null}))
    }

    async fn handle_config_read(&self) -> Result<Value, JsonRpcError> {
        Ok(json!({"config": self.client.get("/config").await.unwrap_or(json!({}))}))
    }

    async fn handle_config_write(&self, params: Value) -> Result<Value, JsonRpcError> {
        let _ = self.client.patch("/config", params).await;
        Ok(json!({}))
    }

    async fn handle_mcp_server_status_list(&self) -> Result<Value, JsonRpcError> {
        Ok(json!({"data": self.client.get("/mcp").await.unwrap_or(json!([]))}))
    }
}

#[async_trait]
impl Bridge for OpencodeBridge {
    async fn initialize(&self, ctx: &Conn, params: Value) -> Result<Value, JsonRpcError> {
        ctx.set_initialize_capabilities(&params);
        Ok(json!({
            "userAgent": concat!("alleycat-opencode-bridge/", env!("CARGO_PKG_VERSION")),
            "codexHome": std::env::temp_dir().join("alleycat-opencode-bridge").to_string_lossy(),
            "platformFamily": std::env::consts::FAMILY,
            "platformOs": std::env::consts::OS
        }))
    }

    async fn dispatch(
        &self,
        ctx: &Conn,
        method: &str,
        params: Value,
    ) -> Result<Value, JsonRpcError> {
        match method {
            "thread/start" => self.handle_thread_start(params).await,
            "thread/list" => self.handle_thread_list(params).await,
            "thread/resume" | "thread/read" => self.handle_thread_resume_or_read(method, params).await,
            "thread/archive" => self.handle_thread_archive(params).await,
            "thread/unarchive" => self.handle_thread_unarchive(params).await,
            "thread/name/set" => self.handle_thread_name_set(params).await,
            "turn/start" => self.handle_turn_start(ctx, params).await,
            "turn/interrupt" => self.handle_turn_interrupt(params).await,
            "turn/steer" => self.handle_turn_start(ctx, params).await,
            "model/list" => self.handle_model_list().await,
            "config/read" => self.handle_config_read().await,
            "config/value/write" | "config/batchWrite" => self.handle_config_write(params).await,
            "mcpServerStatus/list" => self.handle_mcp_server_status_list().await,
            "config/mcpServer/reload" => Ok(json!({})),
            "skills/list" => Ok(json!({"data":[]})),
            "skills/remote/list" | "skills/remote/export" => Ok(json!({"data":[]})),
            "account/read" => Ok(json!({"account":null,"requiresOpenaiAuth":false})),
            "account/rateLimits/read" => Ok(json!({"rateLimits":[]})),
            "account/login/start"
            | "account/login/cancel"
            | "account/logout"
            | "feedback/upload" => Ok(json!({})),
            "experimentalFeature/list" => Ok(json!({"data":[],"nextCursor":null})),
            "collaborationMode/list" => Ok(json!({"data":[]})),
            "thread/loaded/list" => Ok(json!({"threadIds":[]})),
            "thread/backgroundTerminals/clean" => Ok(json!({})),
            "thread/compact/start" => self.handle_thread_compact_start(params).await,
            "thread/rollback" => self.handle_thread_rollback(params).await,
            "thread/fork" => self.handle_thread_fork(params).await,
            "review/start" => Err(JsonRpcError::method_not_found("review/start")),
            "command/exec" => self.handle_command_exec(ctx, params).await,
            "command/exec/write" => self.handle_command_exec_write(params).await,
            "command/exec/terminate" => self.handle_command_exec_terminate(params).await,
            "command/exec/resize" => self.handle_command_exec_resize(params).await,
            "mock/experimentalMethod" => {
                Ok(json!({"echoed":params.get("value").cloned().unwrap_or(Value::Null)}))
            }
            other => Err(JsonRpcError::method_not_found(other)),
        }
    }

    async fn notification(&self, ctx: &Conn, method: &str, _params: Value) {
        if method != "initialized" {
            return;
        }
        self.spawn_event_pump(ctx);
    }
}

impl OpencodeBridge {
    /// `command/exec` — buffered or streaming run of an opencode PTY. The
    /// command is executed via `POST /pty` (which returns immediately) and
    /// the bridge then opens a long-lived websocket against `/pty/{id}/connect`
    /// to capture output. Resolution comes from the SSE `pty.exited` event,
    /// which `route_pty_event` routes back into the per-process exit oneshot.
    ///
    /// Codex's `tty:true` and `streamStdoutStderr:true` both imply chunked
    /// stdout via `command/exec/outputDelta`; opencode's PTY transport is
    /// inherently a single combined stream, so streaming chunks are emitted
    /// with `stream:"stdout"`. `stderr` in the final response is always empty.
    async fn handle_command_exec(
        &self,
        ctx: &Conn,
        params: Value,
    ) -> Result<Value, JsonRpcError> {
        let command = params
            .get("command")
            .and_then(Value::as_array)
            .ok_or_else(|| JsonRpcError::invalid_params("command array is required"))?;
        let program = command
            .first()
            .and_then(Value::as_str)
            .ok_or_else(|| JsonRpcError::invalid_params("command must be non-empty"))?
            .to_string();
        let args = command
            .iter()
            .skip(1)
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            });
        let stream_output = params
            .get("streamStdoutStderr")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || params.get("tty").and_then(Value::as_bool).unwrap_or(false);
        let supplied_process_id = params
            .get("processId")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);

        let create_body = json!({"command":program,"args":args,"cwd":cwd});
        let pty_info = self
            .client
            .pty_create(create_body)
            .await
            .map_err(|err| JsonRpcError::internal(format!("{err:#}")))?;
        let pty_id = pty_info
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| JsonRpcError::internal("opencode /pty returned no id"))?
            .to_string();
        let process_id = supplied_process_id.unwrap_or_else(|| format!("pty-{pty_id}"));

        let process = self
            .pty
            .register(&self.client, process_id.clone(), pty_id.clone());

        // If the caller asked for streaming, fan output deltas out as
        // notifications until either the broadcast closes or the exit fires.
        if stream_output {
            self.spawn_output_delta_pump(ctx, &process);
        }

        let exit_rx = process
            .take_exit_rx()
            .ok_or_else(|| JsonRpcError::internal("pty exit receiver already taken"))?;
        let exit_code = exit_rx.await.unwrap_or(-1);

        let stdout = if stream_output {
            String::new()
        } else {
            String::from_utf8_lossy(&process.snapshot_output()).to_string()
        };

        // Best-effort cleanup: remove the bridge-side registration. The
        // opencode PTY itself is cleaned up by opencode when the process
        // exits, but a `DELETE /pty/{id}` here keeps the registry tidy.
        self.pty.remove(&process_id);
        let _ = self.client.pty_remove(&pty_id).await;

        Ok(json!({
            "exitCode": exit_code,
            "stdout": stdout,
            "stderr": "",
            "processId": process_id,
        }))
    }

    fn spawn_output_delta_pump(&self, ctx: &Conn, process: &Arc<crate::pty::PtyProcess>) {
        let mut rx = process.subscribe_stream();
        let process_id = process.process_id.clone();
        let notifier = ctx.notifier().clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(bytes) => {
                        let _ = notifier.send_notification(
                            "command/exec/outputDelta",
                            crate::pty::output_delta_payload(&process_id, "stdout", &bytes),
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(
                            %process_id,
                            skipped,
                            "command/exec output subscriber lagged",
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    async fn handle_command_exec_write(&self, params: Value) -> Result<Value, JsonRpcError> {
        use base64::Engine;
        let process_id = params
            .get("processId")
            .and_then(Value::as_str)
            .ok_or_else(|| JsonRpcError::invalid_params("processId is required"))?;
        let process = self.pty.get_by_process(process_id).ok_or_else(|| {
            JsonRpcError::invalid_params(format!("unknown processId `{process_id}`"))
        })?;
        if let Some(delta) = params.get("deltaBase64").and_then(Value::as_str) {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(delta)
                .map_err(|err| JsonRpcError::invalid_params(format!("deltaBase64: {err}")))?;
            process
                .write(bytes)
                .map_err(|err| JsonRpcError::internal(format!("{err:#}")))?;
        }
        if params
            .get("closeStdin")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            process
                .close_stdin()
                .map_err(|err| JsonRpcError::internal(format!("{err:#}")))?;
        }
        Ok(json!({}))
    }

    async fn handle_command_exec_terminate(
        &self,
        params: Value,
    ) -> Result<Value, JsonRpcError> {
        let process_id = params
            .get("processId")
            .and_then(Value::as_str)
            .ok_or_else(|| JsonRpcError::invalid_params("processId is required"))?;
        let pty_id = self
            .pty
            .get_by_process(process_id)
            .map(|process| process.pty_id.clone());
        self.pty.remove(process_id);
        if let Some(pty_id) = pty_id {
            let _ = self.client.pty_remove(&pty_id).await;
        }
        Ok(json!({}))
    }

    async fn handle_command_exec_resize(
        &self,
        params: Value,
    ) -> Result<Value, JsonRpcError> {
        let process_id = params
            .get("processId")
            .and_then(Value::as_str)
            .ok_or_else(|| JsonRpcError::invalid_params("processId is required"))?;
        let process = self.pty.get_by_process(process_id).ok_or_else(|| {
            JsonRpcError::invalid_params(format!("unknown processId `{process_id}`"))
        })?;
        let rows = params
            .pointer("/size/rows")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        let cols = params
            .pointer("/size/cols")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        if rows == 0 || cols == 0 {
            return Err(JsonRpcError::invalid_params(
                "size.rows and size.cols must be positive",
            ));
        }
        self.client
            .pty_resize(&process.pty_id, rows, cols)
            .await
            .map_err(|err| JsonRpcError::internal(format!("{err:#}")))?;
        Ok(json!({}))
    }
}

fn binding_from_params(
    index: &ThreadIndex,
    params: &Value,
) -> Result<OpencodeBinding, JsonRpcError> {
    let thread_id = params
        .get("threadId")
        .or_else(|| params.get("thread_id"))
        .and_then(Value::as_str)
        .ok_or_else(|| JsonRpcError::invalid_params("threadId is required"))?;
    index
        .by_thread(thread_id)
        .ok_or_else(|| JsonRpcError::invalid_params("unknown thread"))
}

fn binding_to_thread(binding: &OpencodeBinding) -> Value {
    json!({
        "id": binding.thread_id,
        "forkedFromId": null,
        "preview": binding.preview,
        "ephemeral": false,
        "modelProvider": "opencode",
        "createdAt": binding.created_at,
        "updatedAt": binding.updated_at,
        "status": "notLoaded",
        "path": null,
        "cwd": binding.directory,
        "cliVersion": concat!("alleycat-opencode-bridge/", env!("CARGO_PKG_VERSION")),
        "source": "appServer",
        "gitInfo": null,
        "name": binding.name,
        "turns": []
    })
}

fn permission_from_codex(value: &Value) -> Option<Value> {
    let action = match value.as_str()? {
        "never" => "deny",
        "on-request" | "on-failure" | "untrusted" => "ask",
        _ => "allow",
    };
    Some(json!([{"permission":"*","pattern":"*","action":action}]))
}

fn split_model(model: &str) -> (&str, &str) {
    model.split_once('/').unwrap_or(("opencode", model))
}

fn flatten_models(providers: Value) -> Vec<Value> {
    providers
        .get("all")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|provider| {
            let provider_id = provider.get("id").and_then(Value::as_str).unwrap_or("opencode");
            provider
                .get("models")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(move |model| {
                    let model_id = model.get("id").and_then(Value::as_str).unwrap_or("model");
                    json!({
                        "id": format!("{provider_id}/{model_id}"),
                        "model": model_id,
                        "displayName": model.get("name").and_then(Value::as_str).unwrap_or(model_id),
                        "description": "",
                        "hidden": false,
                        "supportedReasoningEfforts": [],
                        "defaultReasoningEffort": "medium",
                        "inputModalities": ["text"],
                        "supportsPersonality": false,
                        "additionalSpeedTiers": [],
                        "isDefault": false
                    })
                })
        })
        .collect()
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn encode_query(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}
