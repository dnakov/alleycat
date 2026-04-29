//! `UserInput` (input to a turn) and `ThreadItem` (output items in turns).
//! These are the items the bridge translates between pi `AgentMessage`s and
//! codex's history view.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

use super::common::CommandAction;

// === UserInput =============================================================

/// Tagged enum on `type` (camelCase variants). See codex v2.rs:5251.
///
/// Per codex-rs source, the inner variants do **not** carry their own
/// `rename_all`, so multi-word field names stay snake_case on the wire
/// (e.g. `text_elements`, not `textElements`). Match that exactly.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UserInput {
    Text {
        text: String,
        #[serde(default)]
        text_elements: Vec<TextElement>,
    },
    Image {
        url: String,
    },
    LocalImage {
        path: PathBuf,
    },
    Skill {
        name: String,
        path: PathBuf,
    },
    Mention {
        name: String,
        path: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ByteRange {
    pub start: usize,
    pub end: usize,
}

/// UI-defined span inside a `UserInput::Text` buffer. The bridge does not
/// generate these; we round-trip what the client sends.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TextElement {
    pub byte_range: ByteRange,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
}

// === ThreadItem ============================================================

/// `ThreadItem` enum, tagged on `type` (camelCase variants). Matches codex
/// v2.rs:5327.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ThreadItem {
    #[serde(rename_all = "camelCase")]
    UserMessage { id: String, content: Vec<UserInput> },

    #[serde(rename_all = "camelCase")]
    HookPrompt {
        id: String,
        fragments: Vec<HookPromptFragment>,
    },

    #[serde(rename_all = "camelCase")]
    AgentMessage {
        id: String,
        text: String,
        #[serde(default)]
        phase: Option<Value>,
        #[serde(default)]
        memory_citation: Option<Value>,
    },

    #[serde(rename_all = "camelCase")]
    Plan { id: String, text: String },

    #[serde(rename_all = "camelCase")]
    Reasoning {
        id: String,
        #[serde(default)]
        summary: Vec<String>,
        #[serde(default)]
        content: Vec<String>,
    },

    #[serde(rename_all = "camelCase")]
    CommandExecution {
        id: String,
        command: String,
        cwd: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        process_id: Option<String>,
        #[serde(default)]
        source: CommandExecutionSource,
        status: CommandExecutionStatus,
        #[serde(default)]
        command_actions: Vec<CommandAction>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        aggregated_output: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<i64>,
    },

    #[serde(rename_all = "camelCase")]
    FileChange {
        id: String,
        changes: Vec<FileUpdateChange>,
        status: PatchApplyStatus,
    },

    #[serde(rename_all = "camelCase")]
    McpToolCall {
        id: String,
        server: String,
        tool: String,
        status: McpToolCallStatus,
        #[serde(default)]
        arguments: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mcp_app_resource_uri: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Box<McpToolCallResult>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<McpToolCallError>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<i64>,
    },

    #[serde(rename_all = "camelCase")]
    DynamicToolCall {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        tool: String,
        #[serde(default)]
        arguments: Value,
        status: DynamicToolCallStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_items: Option<Vec<Value>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        success: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<i64>,
    },

    /// Collab/subagent tool call (`spawnAgent`, `sendInput`, `resumeAgent`,
    /// `wait`, `closeAgent`). Matches codex v2.rs:5426.
    #[serde(rename_all = "camelCase")]
    CollabAgentToolCall {
        id: String,
        tool: CollabAgentTool,
        status: CollabAgentToolCallStatus,
        sender_thread_id: String,
        #[serde(default)]
        receiver_thread_ids: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// Opaque to the bridge — codex's `ReasoningEffort` enum. Round-tripped
        /// as a JSON value so we don't need to mirror every variant.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_effort: Option<Value>,
        #[serde(default)]
        agents_states: HashMap<String, CollabAgentState>,
    },

    #[serde(rename_all = "camelCase")]
    WebSearch {
        id: String,
        query: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<Value>,
    },

    #[serde(rename_all = "camelCase")]
    ImageView { id: String, path: String },

    #[serde(rename_all = "camelCase")]
    ImageGeneration {
        id: String,
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revised_prompt: Option<String>,
        result: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        saved_path: Option<String>,
    },

    #[serde(rename_all = "camelCase")]
    EnteredReviewMode { id: String, review: String },

    #[serde(rename_all = "camelCase")]
    ExitedReviewMode { id: String, review: String },

    #[serde(rename_all = "camelCase")]
    ContextCompaction { id: String },
}

impl ThreadItem {
    pub fn id(&self) -> &str {
        match self {
            ThreadItem::UserMessage { id, .. }
            | ThreadItem::HookPrompt { id, .. }
            | ThreadItem::AgentMessage { id, .. }
            | ThreadItem::Plan { id, .. }
            | ThreadItem::Reasoning { id, .. }
            | ThreadItem::CommandExecution { id, .. }
            | ThreadItem::FileChange { id, .. }
            | ThreadItem::McpToolCall { id, .. }
            | ThreadItem::DynamicToolCall { id, .. }
            | ThreadItem::CollabAgentToolCall { id, .. }
            | ThreadItem::WebSearch { id, .. }
            | ThreadItem::ImageView { id, .. }
            | ThreadItem::ImageGeneration { id, .. }
            | ThreadItem::EnteredReviewMode { id, .. }
            | ThreadItem::ExitedReviewMode { id, .. }
            | ThreadItem::ContextCompaction { id } => id,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HookPromptFragment {
    pub text: String,
    pub hook_run_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum CommandExecutionSource {
    #[default]
    Agent,
    UserShell,
    UnifiedExecStartup,
    UnifiedExecInteraction,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CommandExecutionStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FileUpdateChange {
    pub path: String,
    pub kind: PatchChangeKind,
    pub diff: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PatchChangeKind {
    Add,
    Delete,
    #[serde(rename_all = "camelCase")]
    Update {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        move_path: Option<PathBuf>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PatchApplyStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum McpToolCallStatus {
    InProgress,
    Completed,
    Failed,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DynamicToolCallStatus {
    InProgress,
    Completed,
    Failed,
}

/// Subagent collab tool name. Matches codex v2.rs:5995.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CollabAgentTool {
    SpawnAgent,
    SendInput,
    ResumeAgent,
    Wait,
    CloseAgent,
}

/// Status of the collab tool call itself (the wrapper). Matches codex
/// v2.rs:6069. Distinct from the per-receiver `CollabAgentStatus` which
/// describes each child agent.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CollabAgentToolCallStatus {
    InProgress,
    Completed,
    Failed,
}

/// Last-known lifecycle state of a target agent. Matches codex v2.rs:6078.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CollabAgentStatus {
    PendingInit,
    Running,
    Interrupted,
    Completed,
    Errored,
    Shutdown,
    NotFound,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CollabAgentState {
    pub status: CollabAgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Result body of a successful MCP tool call. Codex emits this as
/// `CallToolResult` shape under `mcp.rs`. Keep flexible.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpToolCallResult {
    #[serde(default)]
    pub content: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpToolCallError {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn collab_agent_tool_call_round_trip_camel_case() {
        let mut states = HashMap::new();
        states.insert(
            "th_child".to_string(),
            CollabAgentState {
                status: CollabAgentStatus::Running,
                message: Some("warming up".into()),
            },
        );
        let item = ThreadItem::CollabAgentToolCall {
            id: "tool_123".into(),
            tool: CollabAgentTool::SpawnAgent,
            status: CollabAgentToolCallStatus::InProgress,
            sender_thread_id: "th_parent".into(),
            receiver_thread_ids: vec!["th_child".into()],
            prompt: Some("do the thing".into()),
            model: Some("claude-opus-4".into()),
            reasoning_effort: None,
            agents_states: states,
        };
        let v = serde_json::to_value(&item).unwrap();
        assert_eq!(v["type"], "collabAgentToolCall");
        assert_eq!(v["senderThreadId"], "th_parent");
        assert_eq!(v["receiverThreadIds"][0], "th_child");
        assert_eq!(v["agentsStates"]["th_child"]["status"], "running");
        assert_eq!(v["agentsStates"]["th_child"]["message"], "warming up");
        assert_eq!(v["tool"], "spawnAgent");
        assert_eq!(v["status"], "inProgress");
        assert!(v.get("reasoningEffort").is_none(), "None Option must skip");
        let parsed: ThreadItem = serde_json::from_value(v).unwrap();
        assert_eq!(parsed, item);
    }

    #[test]
    fn collab_agent_status_serializes_camel_case() {
        for (status, expected) in [
            (CollabAgentStatus::PendingInit, "pendingInit"),
            (CollabAgentStatus::Running, "running"),
            (CollabAgentStatus::Interrupted, "interrupted"),
            (CollabAgentStatus::Completed, "completed"),
            (CollabAgentStatus::Errored, "errored"),
            (CollabAgentStatus::Shutdown, "shutdown"),
            (CollabAgentStatus::NotFound, "notFound"),
        ] {
            assert_eq!(
                serde_json::to_value(status).unwrap(),
                json!(expected),
                "{status:?}"
            );
        }
    }

    #[test]
    fn collab_agent_tool_id_accessor_works() {
        let item = ThreadItem::CollabAgentToolCall {
            id: "tool_xyz".into(),
            tool: CollabAgentTool::Wait,
            status: CollabAgentToolCallStatus::Completed,
            sender_thread_id: "th_parent".into(),
            receiver_thread_ids: vec![],
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states: HashMap::new(),
        };
        assert_eq!(item.id(), "tool_xyz");
    }
}
