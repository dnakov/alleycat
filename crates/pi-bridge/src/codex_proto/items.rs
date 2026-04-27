//! `UserInput` (input to a turn) and `ThreadItem` (output items in turns).
//! These are the items the bridge translates between pi `AgentMessage`s and
//! codex's history view.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;

use super::common::CommandAction;

// === UserInput =============================================================

/// Tagged enum on `type` (camelCase variants). See codex v2.rs:5251.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UserInput {
    #[serde(rename_all = "camelCase")]
    Text {
        text: String,
        #[serde(default)]
        text_elements: Vec<TextElement>,
    },
    #[serde(rename_all = "camelCase")]
    Image { url: String },
    #[serde(rename_all = "camelCase")]
    LocalImage { path: PathBuf },
    #[serde(rename_all = "camelCase")]
    Skill { name: String, path: PathBuf },
    #[serde(rename_all = "camelCase")]
    Mention { name: String, path: String },
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phase: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
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
