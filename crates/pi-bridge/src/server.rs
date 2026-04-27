//! Reusable connection-driver loop. Both the stdio entry point and the
//! `--socket` / `--listen` daemon path call into [`run_connection`] so the
//! dispatch logic only lives in one place.
//!
//! The function takes the IO halves and the shared pool/index by reference;
//! the caller is responsible for constructing them once and cloning the
//! `Arc`s into each accepted connection.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use alleycat_bridge_core::framing::read_json_line;
use alleycat_bridge_core::framing::write_json_line;
use anyhow::Context;
use anyhow::Result;
use serde_json::Value;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::BufReader;
use tokio::sync::mpsc;

use crate::codex_proto as p;
use crate::handlers;
use crate::pool::PiPool;
use crate::state::ConnectionState;
use crate::state::ThreadDefaults;
use crate::state::ThreadIndexHandle;

/// Drive a single codex client connection from end to end. Returns when the
/// reader half closes cleanly. Errors only surface for genuine IO faults; a
/// malformed JSON-RPC frame is logged and skipped, not fatal.
///
/// The caller supplies the shared pool/index — every accepted connection in
/// daemon mode gets clones of the same `Arc`s, so cwd-bound pi processes,
/// the `~/.codex/pi-bridge/threads.json` index, and the resolved binary path
/// stay consistent across phones.
pub async fn run_connection<R, W>(
    reader: R,
    mut writer: W,
    pi_pool: Arc<PiPool>,
    thread_index: Arc<dyn ThreadIndexHandle>,
    codex_home: PathBuf,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<p::JsonRpcMessage>();
    let state = Arc::new(ConnectionState::new(
        out_tx,
        pi_pool,
        thread_index,
        ThreadDefaults::default(),
    ));

    let writer_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if let Err(err) = write_json_line(&mut writer, &msg).await {
                tracing::error!(%err, "json-rpc write failed; shutting down writer");
                break;
            }
        }
    });

    let mut lines = BufReader::new(reader);
    while let Some(value) = read_json_line::<Value, _>(&mut lines)
        .await
        .context("read json-rpc line")?
    {
        let inbound = match p::InboundMessage::from_value(value) {
            Ok(msg) => msg,
            Err(err) => {
                tracing::warn!(%err, "discarding malformed json-rpc frame");
                continue;
            }
        };
        match inbound {
            p::InboundMessage::Request(req) => {
                let state = Arc::clone(&state);
                let codex_home = codex_home.clone();
                tokio::spawn(async move {
                    handle_request(state, codex_home, req).await;
                });
            }
            p::InboundMessage::Notification(n) => handle_notification(&state, n),
            p::InboundMessage::Response(resp) => {
                let result = match resp.error {
                    Some(err) => Err(crate::state::ServerRequestError::Rpc {
                        code: err.code,
                        message: err.message,
                    }),
                    None => Ok(resp.result.unwrap_or(Value::Null)),
                };
                state.resolve_pending_request(&resp.id, result).await;
            }
        }
    }

    state.cancel_all_pending_requests().await;
    drop(state);
    let _ = writer_task.await;
    Ok(())
}

async fn handle_request(state: Arc<ConnectionState>, codex_home: PathBuf, req: p::JsonRpcRequest) {
    let id = req.id.clone();
    let method = req.method.clone();
    let result = dispatch_request(&state, &codex_home, req).await;
    let frame = match result {
        Ok(value) => p::JsonRpcMessage::Response(p::JsonRpcResponse {
            jsonrpc: p::JsonRpcVersion,
            id,
            result: Some(value),
            error: None,
        }),
        Err(err) => {
            tracing::warn!(%method, %err, "method failed");
            let (code, message) = err.code_and_message();
            p::JsonRpcMessage::Response(p::JsonRpcResponse {
                jsonrpc: p::JsonRpcVersion,
                id,
                result: None,
                error: Some(p::JsonRpcError {
                    code,
                    message,
                    data: None,
                }),
            })
        }
    };
    if let Err(send_err) = state.send(frame) {
        tracing::warn!(%method, ?send_err, "writer task gone; dropping response");
    }
}

async fn dispatch_request(
    state: &Arc<ConnectionState>,
    codex_home: &Path,
    req: p::JsonRpcRequest,
) -> Result<Value, MethodError> {
    let params = req.params.unwrap_or(Value::Null);
    match req.method.as_str() {
        "initialize" => {
            let p_typed: p::InitializeParams = decode(&params)?;
            Ok(serde_json::to_value(
                handlers::lifecycle::handle_initialize(state, p_typed, codex_home),
            )?)
        }
        "account/read" => {
            let p_typed: p::GetAccountParams = if params.is_null() {
                Default::default()
            } else {
                decode(&params)?
            };
            Ok(serde_json::to_value(
                handlers::lifecycle::handle_account_read(state, p_typed),
            )?)
        }
        "account/rateLimits/read" => Ok(serde_json::to_value(
            handlers::lifecycle::handle_account_rate_limits_read(state),
        )?),
        "account/login/start" => {
            let p_typed: p::LoginAccountParams = decode(&params)?;
            Ok(serde_json::to_value(
                handlers::lifecycle::handle_account_login_start(state, p_typed)
                    .map_err(MethodError::internal)?,
            )?)
        }
        "account/login/cancel" => {
            let p_typed: p::CancelLoginAccountParams = decode(&params)?;
            Ok(serde_json::to_value(
                handlers::lifecycle::handle_account_login_cancel(state, p_typed),
            )?)
        }
        "account/logout" => Ok(serde_json::to_value(
            handlers::lifecycle::handle_account_logout(state),
        )?),
        "feedback/upload" => {
            let p_typed: p::FeedbackUploadParams = decode(&params)?;
            Ok(serde_json::to_value(
                handlers::lifecycle::handle_feedback_upload(state, p_typed),
            )?)
        }
        "config/read" => {
            let p_typed: p::ConfigReadParams = if params.is_null() {
                Default::default()
            } else {
                decode(&params)?
            };
            Ok(serde_json::to_value(
                handlers::config::handle_config_read(state, codex_home, p_typed)
                    .map_err(MethodError::internal)?,
            )?)
        }
        "config/value/write" => {
            let p_typed: p::ConfigValueWriteParams = decode(&params)?;
            Ok(serde_json::to_value(
                handlers::config::handle_config_value_write(state, codex_home, p_typed)
                    .map_err(MethodError::internal)?,
            )?)
        }
        "config/batchWrite" => {
            let p_typed: p::ConfigBatchWriteParams = decode(&params)?;
            Ok(serde_json::to_value(
                handlers::config::handle_config_batch_write(state, codex_home, p_typed)
                    .map_err(MethodError::internal)?,
            )?)
        }
        "configRequirements/read" => Ok(serde_json::to_value(
            handlers::config::handle_config_requirements_read(state),
        )?),
        "mcpServerStatus/list" => {
            let p_typed: p::ListMcpServerStatusParams = if params.is_null() {
                Default::default()
            } else {
                decode(&params)?
            };
            Ok(serde_json::to_value(
                handlers::mcp::handle_mcp_server_status_list(state, p_typed),
            )?)
        }
        "config/mcpServer/reload" => Ok(serde_json::to_value(
            handlers::mcp::handle_mcp_server_refresh(state),
        )?),
        "mcpServer/oauth/login" => {
            let p_typed: p::McpServerOauthLoginParams = decode(&params)?;
            Ok(serde_json::to_value(
                handlers::mcp::handle_mcp_server_oauth_login(state, p_typed),
            )?)
        }
        "mock/experimentalMethod" => {
            let p_typed: p::MockExperimentalMethodParams = if params.is_null() {
                Default::default()
            } else {
                decode(&params)?
            };
            Ok(serde_json::to_value(p::MockExperimentalMethodResponse {
                echoed: p_typed.value,
            })?)
        }
        "experimentalFeature/list" => {
            Ok(serde_json::to_value(p::ExperimentalFeatureListResponse {
                data: Vec::new(),
                next_cursor: None,
            })?)
        }
        "collaborationMode/list" => Ok(serde_json::to_value(p::CollaborationModeListResponse {
            data: Vec::new(),
        })?),
        "model/list" => {
            let p_typed: p::ModelListParams = if params.is_null() {
                Default::default()
            } else {
                decode(&params)?
            };
            Ok(serde_json::to_value(
                handlers::model::handle_model_list(state, p_typed).await,
            )?)
        }
        "skills/list" => {
            let p_typed: p::SkillsListParams = if params.is_null() {
                Default::default()
            } else {
                decode(&params)?
            };
            Ok(serde_json::to_value(
                handlers::skills::handle_skills_list(state, p_typed).await,
            )?)
        }
        "skills/remote/list" => Ok(handlers::skills::handle_skills_remote_list(state).await),
        "skills/remote/export" => {
            Ok(handlers::skills::handle_skills_remote_export(state, params).await)
        }
        "skills/config/write" => {
            let p_typed: p::SkillsConfigWriteParams = decode(&params)?;
            Ok(serde_json::to_value(
                handlers::skills::handle_skills_config_write(state, p_typed).await,
            )?)
        }
        "thread/start" => {
            let p_typed: p::ThreadStartParams = decode(&params)?;
            Ok(serde_json::to_value(
                handlers::thread::handle_thread_start(state, p_typed).await?,
            )?)
        }
        "thread/resume" => {
            let p_typed: p::ThreadResumeParams = decode(&params)?;
            Ok(serde_json::to_value(
                handlers::thread::handle_thread_resume(state, p_typed).await?,
            )?)
        }
        "thread/fork" => {
            let p_typed: p::ThreadForkParams = decode(&params)?;
            Ok(serde_json::to_value(
                handlers::thread::handle_thread_fork(state, p_typed).await?,
            )?)
        }
        "thread/archive" => {
            let p_typed: p::ThreadArchiveParams = decode(&params)?;
            Ok(serde_json::to_value(
                handlers::thread::handle_thread_archive(state, p_typed).await?,
            )?)
        }
        "thread/unarchive" => {
            let p_typed: p::ThreadUnarchiveParams = decode(&params)?;
            Ok(serde_json::to_value(
                handlers::thread::handle_thread_unarchive(state, p_typed).await?,
            )?)
        }
        "thread/name/set" => {
            let p_typed: p::ThreadSetNameParams = decode(&params)?;
            Ok(serde_json::to_value(
                handlers::thread::handle_thread_set_name(state, p_typed).await?,
            )?)
        }
        "thread/compact/start" => {
            let p_typed: p::ThreadCompactStartParams = decode(&params)?;
            Ok(serde_json::to_value(
                handlers::thread::handle_thread_compact_start(state, p_typed).await?,
            )?)
        }
        "thread/rollback" => {
            let p_typed: p::ThreadRollbackParams = decode(&params)?;
            Ok(serde_json::to_value(
                handlers::thread::handle_thread_rollback(state, p_typed).await?,
            )?)
        }
        "thread/list" => {
            let p_typed: p::ThreadListParams = if params.is_null() {
                Default::default()
            } else {
                decode(&params)?
            };
            Ok(serde_json::to_value(
                handlers::thread::handle_thread_list(state, p_typed).await?,
            )?)
        }
        "thread/loaded/list" => {
            let p_typed: p::ThreadLoadedListParams = if params.is_null() {
                Default::default()
            } else {
                decode(&params)?
            };
            Ok(serde_json::to_value(
                handlers::thread::handle_thread_loaded_list(state, p_typed).await,
            )?)
        }
        "thread/read" => {
            let p_typed: p::ThreadReadParams = decode(&params)?;
            Ok(serde_json::to_value(
                handlers::thread::handle_thread_read(state, p_typed).await?,
            )?)
        }
        "thread/backgroundTerminals/clean" => {
            let p_typed: p::ThreadBackgroundTerminalsCleanParams = decode(&params)?;
            Ok(serde_json::to_value(
                handlers::thread::handle_thread_background_terminals_clean(state, p_typed).await,
            )?)
        }
        "turn/start" => {
            let p_typed: p::TurnStartParams = decode(&params)?;
            Ok(serde_json::to_value(
                handlers::turn::handle_turn_start(state, p_typed).await?,
            )?)
        }
        "turn/steer" => {
            let p_typed: p::TurnSteerParams = decode(&params)?;
            Ok(serde_json::to_value(
                handlers::turn::handle_turn_steer(state, p_typed).await?,
            )?)
        }
        "turn/interrupt" => {
            let p_typed: p::TurnInterruptParams = decode(&params)?;
            Ok(serde_json::to_value(
                handlers::turn::handle_turn_interrupt(state, p_typed).await?,
            )?)
        }
        "review/start" => {
            let p_typed: p::ReviewStartParams = decode(&params)?;
            Ok(serde_json::to_value(
                handlers::turn::handle_review_start(state, p_typed).await?,
            )?)
        }
        "command/exec" => {
            let p_typed: p::CommandExecParams = decode(&params)?;
            Ok(serde_json::to_value(
                handlers::command_exec::handle_command_exec(state, p_typed).await?,
            )?)
        }
        "command/exec/terminate" => {
            let p_typed: p::CommandExecTerminateParams = decode(&params)?;
            Ok(serde_json::to_value(
                handlers::command_exec::handle_command_exec_terminate(state, p_typed).await,
            )?)
        }
        "command/exec/write" => {
            let p_typed: p::CommandExecWriteParams = decode(&params)?;
            Ok(serde_json::to_value(
                handlers::command_exec::handle_command_exec_write(state, p_typed).await?,
            )?)
        }
        "command/exec/resize" => {
            let p_typed: p::CommandExecResizeParams = decode(&params)?;
            Ok(serde_json::to_value(
                handlers::command_exec::handle_command_exec_resize(state, p_typed).await?,
            )?)
        }
        other => Err(MethodError::method_not_found(other)),
    }
}

fn handle_notification(state: &Arc<ConnectionState>, notif: p::JsonRpcNotification) {
    match notif.method.as_str() {
        "initialized" => handlers::lifecycle::handle_initialized(state),
        other => tracing::debug!(method = %other, "ignoring unknown client notification"),
    }
}

#[derive(Debug, thiserror::Error)]
enum MethodError {
    #[error("method `{0}` is not implemented")]
    MethodNotFound(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("invalid params: {0}")]
    InvalidParams(String),
    #[error("internal error: {0}")]
    Internal(String),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

impl MethodError {
    fn method_not_found(method: &str) -> Self {
        Self::MethodNotFound(method.to_string())
    }

    fn internal<E: std::fmt::Display>(err: E) -> Self {
        Self::Internal(err.to_string())
    }

    fn code_and_message(&self) -> (i64, String) {
        match self {
            MethodError::MethodNotFound(_) | MethodError::Unsupported(_) => {
                (p::error_codes::METHOD_NOT_FOUND, self.to_string())
            }
            MethodError::InvalidParams(_) | MethodError::Serde(_) => {
                (p::error_codes::INVALID_PARAMS, self.to_string())
            }
            MethodError::Internal(_) => (p::error_codes::INTERNAL_ERROR, self.to_string()),
        }
    }
}

fn decode<T: serde::de::DeserializeOwned>(value: &Value) -> Result<T, MethodError> {
    serde_json::from_value(value.clone()).map_err(|err| MethodError::InvalidParams(err.to_string()))
}

impl From<handlers::command_exec::ExecError> for MethodError {
    fn from(err: handlers::command_exec::ExecError) -> Self {
        let message = err.to_string();
        match err.rpc_code() {
            p::error_codes::METHOD_NOT_FOUND => MethodError::Unsupported(message),
            p::error_codes::INVALID_PARAMS => MethodError::InvalidParams(message),
            _ => MethodError::Internal(message),
        }
    }
}

impl From<handlers::thread::ThreadError> for MethodError {
    fn from(err: handlers::thread::ThreadError) -> Self {
        let message = err.to_string();
        match err.rpc_code() {
            p::error_codes::METHOD_NOT_FOUND => MethodError::Unsupported(message),
            p::error_codes::INVALID_PARAMS => MethodError::InvalidParams(message),
            _ => MethodError::Internal(message),
        }
    }
}

impl From<handlers::turn::TurnError> for MethodError {
    fn from(err: handlers::turn::TurnError) -> Self {
        let message = err.to_string();
        match err.rpc_code() {
            p::error_codes::METHOD_NOT_FOUND => MethodError::Unsupported(message),
            p::error_codes::INVALID_PARAMS => MethodError::InvalidParams(message),
            _ => MethodError::Internal(message),
        }
    }
}
