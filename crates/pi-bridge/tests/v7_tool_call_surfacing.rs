//! Verification matrix V7 — tool-call surfacing.
//!
//! From the bridge plan (tool execution rows in "Pi events → codex
//! notifications"): a pi `tool_execution_*` triplet for `toolName:"bash"`
//! must surface as a `CommandExecution` item with output deltas; a triplet
//! whose `toolName` matches the MCP convention `<server>__<tool>` must
//! surface as an `McpToolCall` with progress events.
//!
//! Approach: drive a scripted `prompt` through fake-pi, feed every emitted
//! event through `EventTranslatorState`, and assert the notification stream
//! contains the right shapes for both tool kinds, in order.
//!
//! When `handlers/turn.rs` (#15) lands, an end-to-end variant that drives
//! the same scenario via codex JSON-RPC can sit alongside this one.

mod support;

use std::time::Duration;

use alleycat_pi_bridge::codex_proto::{
    CommandExecutionStatus, McpToolCallStatus, ServerNotification, ThreadItem,
};
use alleycat_pi_bridge::pool::pi_protocol::{PiEvent, PromptCmd, RpcCommand};
use alleycat_pi_bridge::pool::process::PiProcessHandle;
use alleycat_pi_bridge::translate::events::EventTranslatorState;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::timeout;

use support::{fake_pi_path, write_script};

const THREAD_ID: &str = "th_v7";
const TURN_ID: &str = "tu_v7";
const BASH_CALL: &str = "tc_bash";
const MCP_CALL: &str = "tc_mcp";

#[tokio::test]
async fn bash_and_mcp_tool_calls_surface_distinct_codex_items() {
    let cwd = TempDir::new().unwrap();
    let script_dir = TempDir::new().unwrap();

    // Two tool_execution_* cycles inline within one assistant turn.
    // The bash cycle uses pi's actual `bash` tool name; the MCP cycle uses
    // the `<server>__<tool>` convention pi forwards from MCP-bridged tools.
    let script_path = write_script(
        script_dir.path(),
        &[
            json!({"type": "agent_start"}),
            json!({"type": "turn_start"}),
            // ---- bash tool call ----
            json!({
                "type": "tool_execution_start",
                "toolCallId": BASH_CALL,
                "toolName": "bash",
                "args": {"command": "echo hi"},
            }),
            json!({
                "type": "tool_execution_update",
                "toolCallId": BASH_CALL,
                "toolName": "bash",
                "args": {"command": "echo hi"},
                "partialResult": {"stdout": "hi\n"},
            }),
            json!({
                "type": "tool_execution_end",
                "toolCallId": BASH_CALL,
                "toolName": "bash",
                "result": {
                    "output": "hi\n",
                    "exitCode": 0,
                    "cancelled": false,
                    "truncated": false,
                },
                "isError": false,
            }),
            // ---- MCP tool call ----
            json!({
                "type": "tool_execution_start",
                "toolCallId": MCP_CALL,
                "toolName": "github__list_repos",
                "args": {"org": "alleycat-labs"},
            }),
            json!({
                "type": "tool_execution_update",
                "toolCallId": MCP_CALL,
                "toolName": "github__list_repos",
                "args": {"org": "alleycat-labs"},
                "partialResult": {"message": "querying GitHub API"},
            }),
            json!({
                "type": "tool_execution_end",
                "toolCallId": MCP_CALL,
                "toolName": "github__list_repos",
                "result": {"repos": ["a", "b"]},
                "isError": false,
            }),
            json!({"type": "agent_end", "messages": []}),
        ],
    );

    // Safety: cargo serializes tests within a binary by default. We restore
    // the env after spawn so concurrent tests in *other* binaries don't
    // observe our setting.
    unsafe {
        std::env::set_var("FAKE_PI_SCRIPT", &script_path);
    }
    let handle = PiProcessHandle::spawn(cwd.path(), fake_pi_path())
        .await
        .expect("spawn fake-pi");
    unsafe {
        std::env::remove_var("FAKE_PI_SCRIPT");
    }

    let mut events = handle.subscribe_events();
    let mut translator = EventTranslatorState::new(THREAD_ID, TURN_ID);
    let mut notifications: Vec<ServerNotification> = Vec::new();

    let response = handle
        .send_request(RpcCommand::Prompt(PromptCmd {
            id: None,
            message: "go".into(),
            images: Vec::new(),
            streaming_behavior: None,
        }))
        .await
        .expect("prompt");
    assert!(response.success);

    // Drain until we see agent_end. Bound the wait so a regression in
    // fake-pi or the protocol shows up as a timeout rather than a hang.
    loop {
        let evt = match timeout(Duration::from_secs(2), events.recv()).await {
            Ok(Ok(evt)) => evt,
            other => panic!("event stream stalled: {other:?}"),
        };
        let saw_end = matches!(evt, PiEvent::AgentEnd { .. });
        notifications.extend(translator.translate(evt));
        if saw_end {
            break;
        }
    }

    // ---- (a) bash → CommandExecution lifecycle ----
    let bash_started = find_started_command_execution(&notifications, BASH_CALL)
        .expect("expected ItemStarted{CommandExecution} for bash");
    let (cmd_text, cmd_status) = match bash_started {
        ThreadItem::CommandExecution {
            command, status, ..
        } => (command.clone(), *status),
        _ => unreachable!(),
    };
    assert_eq!(
        cmd_text, "echo hi",
        "command should be lifted from args.command"
    );
    assert_eq!(cmd_status, CommandExecutionStatus::InProgress);

    let bash_delta = notifications
        .iter()
        .find_map(|n| match n {
            ServerNotification::CommandExecutionOutputDelta(d) if d.item_id == BASH_CALL => {
                Some(d.delta.as_str())
            }
            _ => None,
        })
        .expect("expected outputDelta for bash");
    assert_eq!(bash_delta, "hi\n");

    let bash_completed = find_completed_command_execution(&notifications, BASH_CALL)
        .expect("expected ItemCompleted{CommandExecution} for bash");
    match bash_completed {
        ThreadItem::CommandExecution {
            status,
            aggregated_output,
            exit_code,
            ..
        } => {
            assert_eq!(*status, CommandExecutionStatus::Completed);
            assert_eq!(aggregated_output.as_deref(), Some("hi\n"));
            assert_eq!(*exit_code, Some(0));
        }
        _ => unreachable!(),
    }

    // ---- (b) MCP → McpToolCall lifecycle ----
    let mcp_started = find_started_mcp(&notifications, MCP_CALL)
        .expect("expected ItemStarted{McpToolCall} for github__list_repos");
    match mcp_started {
        ThreadItem::McpToolCall {
            server,
            tool,
            status,
            arguments,
            ..
        } => {
            assert_eq!(server, "github");
            assert_eq!(tool, "list_repos");
            assert_eq!(*status, McpToolCallStatus::InProgress);
            assert_eq!(arguments["org"], "alleycat-labs");
        }
        _ => unreachable!(),
    }

    let mcp_progress = notifications
        .iter()
        .find_map(|n| match n {
            ServerNotification::McpToolCallProgress(p) if p.item_id == MCP_CALL => {
                Some(p.message.as_str())
            }
            _ => None,
        })
        .expect("expected mcpToolCall/progress");
    assert_eq!(mcp_progress, "querying GitHub API");

    let mcp_completed = find_completed_mcp(&notifications, MCP_CALL)
        .expect("expected ItemCompleted{McpToolCall} for github__list_repos");
    match mcp_completed {
        ThreadItem::McpToolCall {
            server,
            tool,
            status,
            result,
            error,
            ..
        } => {
            assert_eq!(server, "github");
            assert_eq!(tool, "list_repos");
            assert_eq!(*status, McpToolCallStatus::Completed);
            assert!(result.is_some(), "successful MCP call must carry result");
            assert!(error.is_none(), "successful MCP call must not carry error");
        }
        _ => unreachable!(),
    }

    // Ordering sanity: each tool's started < its completed; the two tools
    // are interleaved in the script with bash entirely before MCP, so the
    // bash-completed must precede mcp-started.
    let pos_bash_start = position_of(
        &notifications,
        |n| matches!(n, ServerNotification::ItemStarted(s) if id_of(&s.item) == BASH_CALL),
    );
    let pos_bash_done = position_of(
        &notifications,
        |n| matches!(n, ServerNotification::ItemCompleted(c) if id_of(&c.item) == BASH_CALL),
    );
    let pos_mcp_start = position_of(
        &notifications,
        |n| matches!(n, ServerNotification::ItemStarted(s) if id_of(&s.item) == MCP_CALL),
    );
    let pos_mcp_done = position_of(
        &notifications,
        |n| matches!(n, ServerNotification::ItemCompleted(c) if id_of(&c.item) == MCP_CALL),
    );
    assert!(pos_bash_start < pos_bash_done);
    assert!(pos_bash_done < pos_mcp_start);
    assert!(pos_mcp_start < pos_mcp_done);

    // No spurious errors on the success path.
    assert!(
        !notifications
            .iter()
            .any(|n| matches!(n, ServerNotification::Error(_))),
        "successful tool calls must not emit Error notifications"
    );

    handle.shutdown().await;
}

// ---- helpers ----

fn find_started_command_execution<'a>(
    notifications: &'a [ServerNotification],
    item_id: &str,
) -> Option<&'a ThreadItem> {
    notifications.iter().find_map(|n| match n {
        ServerNotification::ItemStarted(s) => match &s.item {
            ThreadItem::CommandExecution { id, .. } if id == item_id => Some(&s.item),
            _ => None,
        },
        _ => None,
    })
}

fn find_completed_command_execution<'a>(
    notifications: &'a [ServerNotification],
    item_id: &str,
) -> Option<&'a ThreadItem> {
    notifications.iter().find_map(|n| match n {
        ServerNotification::ItemCompleted(c) => match &c.item {
            ThreadItem::CommandExecution { id, .. } if id == item_id => Some(&c.item),
            _ => None,
        },
        _ => None,
    })
}

fn find_started_mcp<'a>(
    notifications: &'a [ServerNotification],
    item_id: &str,
) -> Option<&'a ThreadItem> {
    notifications.iter().find_map(|n| match n {
        ServerNotification::ItemStarted(s) => match &s.item {
            ThreadItem::McpToolCall { id, .. } if id == item_id => Some(&s.item),
            _ => None,
        },
        _ => None,
    })
}

fn find_completed_mcp<'a>(
    notifications: &'a [ServerNotification],
    item_id: &str,
) -> Option<&'a ThreadItem> {
    notifications.iter().find_map(|n| match n {
        ServerNotification::ItemCompleted(c) => match &c.item {
            ThreadItem::McpToolCall { id, .. } if id == item_id => Some(&c.item),
            _ => None,
        },
        _ => None,
    })
}

fn id_of(item: &ThreadItem) -> &str {
    item.id()
}

fn position_of(
    notifications: &[ServerNotification],
    pred: impl Fn(&ServerNotification) -> bool,
) -> usize {
    notifications
        .iter()
        .position(pred)
        .expect("predicate did not match any notification")
}
