//! Translate one opencode `tool` part into the codex `ThreadItem` shape the
//! client should render. Inline string-match dispatch — no enum classifier
//! crate. Each branch maps directly to one canonical shape.
//!
//! | opencode `tool`                      | codex item type                                 |
//! |--------------------------------------|--------------------------------------------------|
//! | `bash`                               | `commandExecution` (existing)                    |
//! | `write` / `edit` / `patch` / `apply_patch` | `fileChange` (existing)                    |
//! | `<server>__<name>`                   | `mcpToolCall` (existing)                         |
//! | `read`                               | `commandExecution` (read action)                 |
//! | `glob`                               | `commandExecution` (list_files action)           |
//! | `grep`                               | `commandExecution` (search action)               |
//! | `codesearch`                         | `commandExecution` (search action)               |
//! | `websearch`                          | `webSearch`                                      |
//! | `task`                               | `collabAgentToolCall`                            |
//! | `todowrite`, `question`              | no live item; historical dynamic item            |
//! | anything else (`webfetch`, `codesearch`, `skill`, ...) | `dynamicToolCall`              |
//!
//! Live `todowrite` and `question` return `None` from this function because
//! their payloads ride a different codex channel:
//! - `todowrite` → emit `turn/plan/updated` via [`tool_part_side_notifications`].
//! - `question` → live SSE handler `handle_question_asked` dispatches
//!   `item/tool/requestUserInput`. Suppressing the dynamic-tool item here
//!   avoids a duplicate card. History hydration opts back into a dynamic item
//!   so past questions/todos remain visible.

use serde_json::{Value, json};
use std::path::Path;

/// Hard cap on `aggregatedOutput` for read/grep/glob results so a multi-MB
/// file body doesn't bloat every notification round-trip.
const EXPLORATION_OUTPUT_CAP: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, Default)]
pub struct ToolPartContext<'a> {
    pub cwd: Option<&'a str>,
    pub sender_thread_id: Option<&'a str>,
    pub include_side_channel_items: bool,
}

pub fn tool_part_to_item(part: &Value) -> Option<Value> {
    tool_part_to_item_with_context(part, ToolPartContext::default())
}

pub fn tool_part_to_item_with_context(part: &Value, context: ToolPartContext<'_>) -> Option<Value> {
    let id = part
        .get("callID")
        .or_else(|| part.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let tool = part.get("tool").and_then(Value::as_str).unwrap_or("tool");
    let state = part.get("state").cloned().unwrap_or(Value::Null);
    let status = normalize_operation_status(state.get("status").and_then(Value::as_str));
    let input = state.get("input").cloned().unwrap_or(Value::Null);

    if tool == "bash" {
        let command = input.get("command").and_then(Value::as_str).unwrap_or("");
        let mut item = json!({
            "type": "commandExecution",
            "id": id,
            "command": command,
            "cwd": resolved_cwd(&input, context),
            "status": command_status(status),
            "aggregatedOutput": cap_output(extract_text_output(&state)),
        });
        add_exit_code(&mut item, &state);
        add_duration(&mut item, &state);
        return Some(item);
    }
    if matches!(tool, "write" | "edit" | "patch" | "apply_patch") {
        return Some(json!({
            "type": "fileChange",
            "id": id,
            "changes": synthesize_file_changes(tool, &state, &input),
            "status": patch_status(status),
        }));
    }
    if let Some((server, name)) = tool.split_once("__") {
        let mut item = json!({
            "type": "mcpToolCall",
            "id": id,
            "server": server,
            "tool": name,
            "arguments": input,
            "status": mcp_status(status),
        });
        add_duration(&mut item, &state);
        add_mcp_result_or_error(&mut item, &state, status);
        return Some(item);
    }

    match tool {
        "read" => {
            let path = input
                .get("filePath")
                .or_else(|| input.get("path"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let cwd = resolved_cwd(&input, context);
            let command = format!("read {path}");
            let mut item = json!({
                "type": "commandExecution",
                "id": id,
                "command": command,
                "cwd": cwd,
                "status": command_status(status),
                "commandActions": [{
                    "type": "read",
                    "command": command,
                    "name": path,
                    "path": absolute_path_or_join(path, &cwd),
                }],
                "aggregatedOutput": cap_output(extract_text_output(&state)),
            });
            add_duration(&mut item, &state);
            Some(item)
        }
        "glob" => {
            let cwd = resolved_cwd(&input, context);
            let path = input.get("path").and_then(Value::as_str);
            let pattern = input.get("pattern").and_then(Value::as_str);
            let display = match (pattern, path) {
                (Some(p), Some(d)) => format!("glob {p} {d}"),
                (Some(p), None) => format!("glob {p}"),
                (None, Some(d)) => format!("glob {d}"),
                _ => "glob".to_string(),
            };
            let mut action = json!({"type": "listFiles", "command": display});
            if let Some(p) = path {
                action["path"] = Value::String(p.to_string());
            }
            let mut item = json!({
                "type": "commandExecution",
                "id": id,
                "command": display,
                "cwd": cwd,
                "status": command_status(status),
                "commandActions": [action],
                "aggregatedOutput": cap_output(extract_text_output(&state)),
            });
            add_duration(&mut item, &state);
            Some(item)
        }
        "grep" => {
            let cwd = resolved_cwd(&input, context);
            let pattern = input.get("pattern").and_then(Value::as_str).unwrap_or("");
            let path = input.get("path").and_then(Value::as_str);
            let command = match path {
                Some(path) if !path.is_empty() => format!("grep {pattern} {path}"),
                _ => format!("grep {pattern}"),
            };
            let mut action = json!({"type": "search", "command": command, "query": pattern});
            if let Some(p) = path {
                action["path"] = Value::String(p.to_string());
            }
            let mut item = json!({
                "type": "commandExecution",
                "id": id,
                "command": command,
                "cwd": cwd,
                "status": command_status(status),
                "commandActions": [action],
                "aggregatedOutput": cap_output(extract_text_output(&state)),
            });
            add_duration(&mut item, &state);
            Some(item)
        }
        "codesearch" => {
            let cwd = resolved_cwd(&input, context);
            let query = input.get("query").and_then(Value::as_str).unwrap_or("");
            let command = format!("codesearch {query}");
            let mut item = json!({
                "type": "commandExecution",
                "id": id,
                "command": command,
                "cwd": cwd,
                "status": command_status(status),
                "commandActions": [{
                    "type": "search",
                    "command": command,
                    "query": query,
                }],
                "aggregatedOutput": cap_output(extract_text_output(&state)),
            });
            add_duration(&mut item, &state);
            Some(item)
        }
        "websearch" => {
            let query = input.get("query").and_then(Value::as_str).unwrap_or("");
            Some(json!({
                "type": "webSearch",
                "id": id,
                "query": query,
                "action": {"type": "search", "query": query},
            }))
        }
        "list" | "ls" => {
            let cwd = resolved_cwd(&input, context);
            let path = input.get("path").and_then(Value::as_str).unwrap_or(".");
            let command = if tool == "ls" {
                format!("ls {path}")
            } else {
                format!("list {path}")
            };
            let mut item = json!({
                "type": "commandExecution",
                "id": id,
                "command": command,
                "cwd": cwd,
                "status": command_status(status),
                "commandActions": [{
                    "type": "listFiles",
                    "command": command,
                    "path": path,
                }],
                "aggregatedOutput": cap_output(extract_text_output(&state)),
            });
            add_duration(&mut item, &state);
            Some(item)
        }
        "task" => {
            let prompt = input.get("prompt").and_then(Value::as_str);
            let subagent_type = input.get("subagent_type").and_then(Value::as_str);
            let description = input.get("description").and_then(Value::as_str);
            let label = task_label(subagent_type, description);
            let receiver_id = state
                .pointer("/metadata/sessionId")
                .or_else(|| state.pointer("/metadata/sessionID"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("subagent-{id}"));
            let mut agents_states = serde_json::Map::new();
            let mut state_obj = json!({"status": collab_agent_status(status)});
            let message = state
                .pointer("/metadata/summary")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
                .or(label);
            if let Some(message) = message {
                state_obj["message"] = Value::String(message);
            }
            agents_states.insert(receiver_id.clone(), state_obj);
            let mut item = json!({
                "type": "collabAgentToolCall",
                "id": id,
                "tool": "spawnAgent",
                "status": collab_status(status),
                "senderThreadId": context.sender_thread_id.unwrap_or(""),
                "receiverThreadIds": [receiver_id],
                "agentsStates": agents_states,
            });
            if let Some(p) = prompt {
                item["prompt"] = Value::String(p.to_string());
            }
            add_duration(&mut item, &state);
            Some(item)
        }
        "todowrite" | "question" if !context.include_side_channel_items => None,
        _ => Some(dynamic_tool_item(id, tool, &state, &input, status)),
    }
}

pub fn tool_part_status_is_terminal(part: &Value) -> bool {
    let Some(raw) = part.pointer("/state/status").and_then(Value::as_str) else {
        return false;
    };
    matches!(
        normalize_operation_status(Some(raw)),
        OperationStatus::Completed | OperationStatus::Failed | OperationStatus::Declined
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OperationStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
}

fn normalize_operation_status(raw: Option<&str>) -> OperationStatus {
    let normalized = raw.unwrap_or("completed").trim().to_ascii_lowercase();
    match normalized.as_str() {
        "pending" | "running" | "queued" | "started" | "in_progress" | "in-progress"
        | "inprogress" => OperationStatus::InProgress,
        "completed" | "complete" | "success" | "succeeded" | "done" => OperationStatus::Completed,
        "declined" | "denied" | "rejected" => OperationStatus::Declined,
        "error" | "failed" | "failure" | "cancelled" | "canceled" | "aborted" => {
            OperationStatus::Failed
        }
        _ => OperationStatus::Completed,
    }
}

fn command_status(status: OperationStatus) -> &'static str {
    match status {
        OperationStatus::InProgress => "inProgress",
        OperationStatus::Completed => "completed",
        OperationStatus::Failed => "failed",
        OperationStatus::Declined => "declined",
    }
}

fn patch_status(status: OperationStatus) -> &'static str {
    command_status(status)
}

fn mcp_status(status: OperationStatus) -> &'static str {
    match status {
        OperationStatus::InProgress => "inProgress",
        OperationStatus::Completed => "completed",
        OperationStatus::Failed | OperationStatus::Declined => "failed",
    }
}

fn dynamic_status(status: OperationStatus) -> &'static str {
    mcp_status(status)
}

fn collab_status(status: OperationStatus) -> &'static str {
    mcp_status(status)
}

fn collab_agent_status(status: OperationStatus) -> &'static str {
    match status {
        OperationStatus::InProgress => "running",
        OperationStatus::Completed => "completed",
        OperationStatus::Failed | OperationStatus::Declined => "errored",
    }
}

fn resolved_cwd(input: &Value, context: ToolPartContext<'_>) -> String {
    input
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| context.cwd.filter(|s| !s.is_empty()))
        .map(ToOwned::to_owned)
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|path| path.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "/".to_string())
}

fn absolute_path_or_join(path: &str, cwd: &str) -> String {
    if path.is_empty() {
        return cwd.to_string();
    }
    let path_ref = Path::new(path);
    if path_ref.is_absolute() {
        return path.to_string();
    }
    Path::new(cwd).join(path_ref).to_string_lossy().into_owned()
}

fn add_duration(item: &mut Value, state: &Value) {
    if let Some(duration_ms) = duration_ms(state) {
        item["durationMs"] = json!(duration_ms);
    }
}

fn duration_ms(state: &Value) -> Option<i64> {
    let start = state.pointer("/time/start").and_then(Value::as_i64)?;
    let end = state.pointer("/time/end").and_then(Value::as_i64)?;
    (end >= start).then_some(end - start)
}

fn add_exit_code(item: &mut Value, state: &Value) {
    if let Some(exit_code) = state
        .pointer("/metadata/exit")
        .and_then(Value::as_i64)
        .or_else(|| state.get("exit").and_then(Value::as_i64))
        && let Ok(exit_code) = i32::try_from(exit_code)
    {
        item["exitCode"] = json!(exit_code);
    }
}

fn add_mcp_result_or_error(item: &mut Value, state: &Value, status: OperationStatus) {
    if matches!(status, OperationStatus::Failed | OperationStatus::Declined) {
        let message = extract_text_output(state);
        item["error"] = json!({
            "message": if message.is_empty() { "Tool failed".to_string() } else { message },
        });
        return;
    }
    let text = extract_text_output(state);
    if !text.is_empty() {
        item["result"] = json!({
            "content": [{"type": "text", "text": cap_output(text)}],
        });
    }
}

fn dynamic_tool_item(
    id: &str,
    tool: &str,
    state: &Value,
    input: &Value,
    status: OperationStatus,
) -> Value {
    let mut item = json!({
        "type": "dynamicToolCall",
        "id": id,
        "tool": tool,
        "arguments": input,
        "status": dynamic_status(status),
    });
    if !matches!(status, OperationStatus::InProgress) {
        item["success"] = json!(matches!(status, OperationStatus::Completed));
    }
    add_duration(&mut item, state);
    let output = tool_output_text(tool, state, input);
    if !output.is_empty() {
        item["contentItems"] = json!([{
            "type": "inputText",
            "text": cap_output(output),
        }]);
    }
    item
}

fn task_label(subagent_type: Option<&str>, description: Option<&str>) -> Option<String> {
    match (subagent_type, description) {
        (Some(t), Some(d)) if !t.is_empty() && !d.is_empty() => Some(format!("{t}: {d}")),
        (Some(t), _) if !t.is_empty() => Some(t.to_string()),
        (_, Some(d)) if !d.is_empty() => Some(d.to_string()),
        _ => None,
    }
}

fn tool_output_text(tool: &str, state: &Value, input: &Value) -> String {
    match tool {
        "todowrite" => {
            let text = todos_to_text(input);
            if text.is_empty() {
                extract_text_output(state)
            } else {
                text
            }
        }
        "question" => {
            let text = questions_to_text(input, state);
            if text.is_empty() {
                extract_text_output(state)
            } else {
                text
            }
        }
        _ => extract_text_output(state),
    }
}

fn todos_to_text(input: &Value) -> String {
    let Some(todos) = input.get("todos").and_then(Value::as_array) else {
        return String::new();
    };
    todos
        .iter()
        .filter_map(|todo| {
            let content = todo.get("content").and_then(Value::as_str)?;
            let status = todo
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("pending");
            Some(format!("- [{status}] {content}"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn questions_to_text(input: &Value, state: &Value) -> String {
    let mut lines = Vec::new();
    if let Some(questions) = input.get("questions").and_then(Value::as_array) {
        for question in questions {
            if let Some(text) = question.get("question").and_then(Value::as_str) {
                lines.push(format!("Q: {text}"));
            }
        }
    }
    if let Some(answers) = state.pointer("/metadata/answers") {
        lines.push(format!("A: {}", compact_json(answers)));
    }
    lines.join("\n")
}

/// Side-channel notifications a tool part should also emit (besides any
/// `ThreadItem` returned by [`tool_part_to_item`]). Caller wraps each
/// `(method, params)` pair into a JSON-RPC notification.
///
/// Currently produces:
/// - `turn/plan/updated` for `todowrite` (bulk-replace todo list).
///
/// Returns an empty Vec for tools without side-effects.
pub fn tool_part_side_notifications(
    part: &Value,
    thread_id: &str,
    turn_id: &str,
) -> Vec<(&'static str, Value)> {
    let tool = part.get("tool").and_then(Value::as_str).unwrap_or("");
    if tool != "todowrite" {
        return Vec::new();
    }
    let Some(todos) = part.pointer("/state/input/todos").and_then(Value::as_array) else {
        return Vec::new();
    };
    let plan: Vec<Value> = todos
        .iter()
        .map(|t| {
            let step = t
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let status = match t.get("status").and_then(Value::as_str) {
                Some("in_progress") => "inProgress",
                Some("completed") => "completed",
                _ => "pending",
            };
            json!({"step": step, "status": status})
        })
        .collect();
    vec![(
        "turn/plan/updated",
        json!({
            "threadId": thread_id,
            "turnId": turn_id,
            "plan": plan,
        }),
    )]
}

/// Build a `FileUpdateChange[]` payload for opencode `write` / `edit` /
/// `patch` / `apply_patch` tool parts. Returns the raw JSON-array shape
/// the wire expects (camelCase fields, `kind: {type: "add"|"update"}`).
///
/// Opencode's `edit` tool already includes a fully-formed unified diff at
/// `state.metadata.diff` — we lift it directly when present. `write`
/// only carries `{path, content}` so we synthesize an additions-only
/// hunk. `patch` / `apply_patch` are pass-through if they carry a diff.
fn synthesize_file_changes(tool: &str, state: &Value, input: &Value) -> Value {
    let path = input
        .get("filePath")
        .or_else(|| input.get("path"))
        .or_else(|| input.get("file_path"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if path.is_empty() {
        return json!([]);
    }
    match tool {
        "write" => {
            let content = input.get("content").and_then(Value::as_str).unwrap_or("");
            json!([{
                "path": path,
                "kind": {"type": "add"},
                "diff": unified_addition(content),
            }])
        }
        "edit" => {
            // Prefer the canonical metadata.diff (a full unified diff
            // opencode synthesizes itself). Fall back to a hand-rolled
            // hunk built from oldString/newString.
            let diff = state
                .pointer("/metadata/diff")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    let old = input.get("oldString").and_then(Value::as_str).unwrap_or("");
                    let new = input.get("newString").and_then(Value::as_str).unwrap_or("");
                    unified_hunk(old, new)
                });
            json!([{
                "path": path,
                "kind": {"type": "update"},
                "diff": diff,
            }])
        }
        "patch" | "apply_patch" => {
            let diff = state
                .pointer("/metadata/diff")
                .and_then(Value::as_str)
                .or_else(|| input.get("patch").and_then(Value::as_str))
                .or_else(|| input.get("diff").and_then(Value::as_str))
                .unwrap_or("")
                .to_string();
            json!([{
                "path": path,
                "kind": {"type": "update"},
                "diff": diff,
            }])
        }
        _ => json!([]),
    }
}

fn unified_hunk(old: &str, new: &str) -> String {
    let old_count = old.lines().count().max(1);
    let new_count = new.lines().count().max(1);
    let mut out = format!("@@ -1,{old_count} +1,{new_count} @@\n");
    for line in old.lines() {
        out.push('-');
        out.push_str(line);
        out.push('\n');
    }
    for line in new.lines() {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn unified_addition(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let count = lines.len().max(1);
    let mut out = format!("@@ -0,0 +1,{count} @@\n");
    for line in &lines {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn cap_output(text: String) -> String {
    if text.len() <= EXPLORATION_OUTPUT_CAP {
        return text;
    }
    let mut idx = EXPLORATION_OUTPUT_CAP;
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    let mut truncated = text;
    truncated.truncate(idx);
    truncated.push_str("\n... [truncated]");
    truncated
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

/// Pull the text body out of a tool's `state.output` field. Opencode emits
/// `output` as a string for most tools; some carry it under
/// `state.metadata` or as a content array. Returns "" if no recognizable
/// shape is present.
fn extract_text_output(state: &Value) -> String {
    if let Some(s) = state.get("output").and_then(Value::as_str) {
        return s.to_string();
    }
    if let Some(arr) = state.get("output").and_then(Value::as_array) {
        let mut joined = String::new();
        for entry in arr {
            if let Some(text) = entry.get("text").and_then(Value::as_str) {
                if !joined.is_empty() && !joined.ends_with('\n') {
                    joined.push('\n');
                }
                joined.push_str(text);
            }
        }
        return joined;
    }
    let mut joined = Vec::new();
    for pointer in [
        "/metadata/output",
        "/metadata/stdout",
        "/metadata/stderr",
        "/metadata/preview",
        "/metadata/summary",
    ] {
        if let Some(text) = state.pointer(pointer).and_then(Value::as_str)
            && !text.is_empty()
        {
            joined.push(text.to_string());
        }
    }
    if !joined.is_empty() {
        return joined.join("\n");
    }
    for pointer in [
        "/metadata/matches",
        "/metadata/answers",
        "/metadata/todos",
        "/metadata/diagnostics",
    ] {
        if let Some(value) = state.pointer(pointer)
            && !value.is_null()
        {
            return compact_json(value);
        }
    }
    if let Some(error) = state.get("error").and_then(Value::as_str) {
        return error.to_string();
    }
    if let Some(error) = state.get("error")
        && !error.is_null()
    {
        return compact_json(error);
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(tool: &str, input: Value, output: Option<&str>) -> Value {
        let mut state = json!({"status": "completed", "input": input});
        if let Some(o) = output {
            state["output"] = Value::String(o.to_string());
        }
        json!({
            "callID": format!("call-{tool}"),
            "tool": tool,
            "state": state,
        })
    }

    #[test]
    fn bash_remains_canonical() {
        let p = part(
            "bash",
            json!({"command": "echo hi", "cwd": "/tmp"}),
            Some("hi\n"),
        );
        let item = tool_part_to_item(&p).unwrap();
        assert_eq!(item["type"], "commandExecution");
        assert_eq!(item["command"], "echo hi");
        assert_eq!(item["aggregatedOutput"], "hi\n");
    }

    #[test]
    fn write_and_edit_remain_filechange() {
        for t in ["write", "edit", "patch", "apply_patch"] {
            // Provide minimal args so synthesize_file_changes returns a
            // populated entry rather than the empty-path fallback.
            let item = tool_part_to_item(&part(t, json!({"path": "/x"}), None)).unwrap();
            assert_eq!(item["type"], "fileChange", "{t}");
        }
    }

    #[test]
    fn write_filechange_carries_addition_diff() {
        let item = tool_part_to_item(&part(
            "write",
            json!({"path": "/tmp/new.txt", "content": "alpha\nbeta\n"}),
            None,
        ))
        .unwrap();
        let changes = item["changes"].as_array().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0]["path"], "/tmp/new.txt");
        assert_eq!(changes[0]["kind"]["type"], "add");
        let diff = changes[0]["diff"].as_str().unwrap();
        assert!(diff.starts_with("@@ -0,0 +1,2 @@"));
        assert!(diff.contains("+alpha"));
        assert!(diff.contains("+beta"));
    }

    #[test]
    fn edit_filechange_uses_metadata_diff_when_present() {
        // Real opencode edit results carry a fully-formed unified diff in
        // state.metadata.diff. The bridge must lift it through verbatim.
        let canonical_diff = "Index: /x\n===================================================================\n--- /x\n+++ /x\n@@ -1 +1 @@\n-foo\n+bar\n";
        let part_value = json!({
            "callID": "call-edit",
            "tool": "edit",
            "state": {
                "status": "completed",
                "input": {"filePath": "/x", "oldString": "foo", "newString": "bar"},
                "metadata": {"diff": canonical_diff}
            }
        });
        let item = tool_part_to_item(&part_value).unwrap();
        let changes = item["changes"].as_array().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0]["path"], "/x");
        assert_eq!(changes[0]["kind"]["type"], "update");
        assert_eq!(changes[0]["diff"], canonical_diff);
    }

    #[test]
    fn edit_filechange_falls_back_to_synthesized_hunk() {
        // When metadata.diff is absent we hand-roll a hunk from
        // oldString/newString.
        let part_value = json!({
            "callID": "call-edit2",
            "tool": "edit",
            "state": {
                "status": "completed",
                "input": {"filePath": "/x", "oldString": "foo", "newString": "bar"}
            }
        });
        let item = tool_part_to_item(&part_value).unwrap();
        let changes = item["changes"].as_array().unwrap();
        let diff = changes[0]["diff"].as_str().unwrap();
        assert!(diff.contains("-foo"));
        assert!(diff.contains("+bar"));
    }

    #[test]
    fn filechange_with_no_path_returns_empty_changes() {
        // Some opencode tool variants don't include filePath/path in
        // input (eg patch parts without explicit path). Fall through to
        // empty changes rather than synthesizing a bogus path="" entry.
        let item = tool_part_to_item(&part("write", json!({"content": "x"}), None)).unwrap();
        assert_eq!(item["type"], "fileChange");
        assert_eq!(item["changes"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn read_emits_command_execution_with_read_action() {
        let p = part("read", json!({"filePath": "/tmp/x"}), Some("hello"));
        let item = tool_part_to_item(&p).unwrap();
        assert_eq!(item["type"], "commandExecution");
        assert_eq!(item["command"], "read /tmp/x");
        assert_eq!(item["commandActions"][0]["type"], "read");
        assert_eq!(item["commandActions"][0]["command"], "read /tmp/x");
        assert_eq!(item["commandActions"][0]["name"], "/tmp/x");
        assert_eq!(item["commandActions"][0]["path"], "/tmp/x");
        assert_eq!(item["aggregatedOutput"], "hello");
    }

    #[test]
    fn read_falls_back_to_path_when_file_path_missing() {
        let p = part("read", json!({"path": "/x.txt"}), Some("body"));
        let item = tool_part_to_item(&p).unwrap();
        assert_eq!(item["commandActions"][0]["path"], "/x.txt");
    }

    #[test]
    fn glob_emits_list_files_action() {
        let p = part("glob", json!({"pattern": "**/*.md", "path": "/repo"}), None);
        let item = tool_part_to_item(&p).unwrap();
        assert_eq!(item["type"], "commandExecution");
        assert_eq!(item["command"], "glob **/*.md /repo");
        assert_eq!(item["commandActions"][0]["type"], "listFiles");
        assert_eq!(item["commandActions"][0]["command"], "glob **/*.md /repo");
        assert_eq!(item["commandActions"][0]["path"], "/repo");
    }

    #[test]
    fn grep_emits_search_action() {
        let p = part("grep", json!({"pattern": "TODO", "path": "src"}), None);
        let item = tool_part_to_item(&p).unwrap();
        assert_eq!(item["type"], "commandExecution");
        assert_eq!(item["command"], "grep TODO src");
        assert_eq!(item["commandActions"][0]["type"], "search");
        assert_eq!(item["commandActions"][0]["command"], "grep TODO src");
        assert_eq!(item["commandActions"][0]["query"], "TODO");
        assert_eq!(item["commandActions"][0]["path"], "src");
    }

    #[test]
    fn codesearch_emits_command_execution_search_action() {
        let p = part(
            "codesearch",
            json!({"query": "symbol:MobileClient", "tokensNum": 4000}),
            Some("shared/rust-bridge/codex-mobile-client/src/lib.rs"),
        );
        let item = tool_part_to_item_with_context(
            &p,
            ToolPartContext {
                cwd: Some("/repo"),
                sender_thread_id: None,
                include_side_channel_items: false,
            },
        )
        .unwrap();
        assert_eq!(item["type"], "commandExecution");
        assert_eq!(item["cwd"], "/repo");
        assert_eq!(item["command"], "codesearch symbol:MobileClient");
        assert_eq!(item["commandActions"][0]["type"], "search");
        assert_eq!(item["commandActions"][0]["query"], "symbol:MobileClient");
        assert_eq!(
            item["aggregatedOutput"],
            "shared/rust-bridge/codex-mobile-client/src/lib.rs"
        );
    }

    #[test]
    fn websearch_emits_web_search_item() {
        let p = part("websearch", json!({"query": "rust async"}), Some("result"));
        let item = tool_part_to_item(&p).unwrap();
        assert_eq!(item["type"], "webSearch");
        assert_eq!(item["query"], "rust async");
        assert_eq!(item["action"]["type"], "search");
        assert_eq!(item["action"]["query"], "rust async");
    }

    #[test]
    fn task_emits_collab_agent_tool_call() {
        let p = part(
            "task",
            json!({
                "prompt": "Say hello",
                "subagent_type": "general",
                "description": "Test agent"
            }),
            None,
        );
        let item = tool_part_to_item(&p).unwrap();
        assert_eq!(item["type"], "collabAgentToolCall");
        assert_eq!(item["tool"], "spawnAgent");
        assert_eq!(item["prompt"], "Say hello");
        assert_eq!(item["receiverThreadIds"][0], "subagent-call-task");
        assert_eq!(
            item["agentsStates"]["subagent-call-task"]["message"],
            "general: Test agent"
        );
    }

    #[test]
    fn command_tools_use_context_cwd_status_exit_and_duration() {
        let part_value = json!({
            "callID": "call-bash",
            "tool": "bash",
            "state": {
                "status": "error",
                "input": {"command": "false"},
                "output": "boom",
                "metadata": {"exit": 1},
                "time": {"start": 1000, "end": 1250}
            }
        });
        let item = tool_part_to_item_with_context(
            &part_value,
            ToolPartContext {
                cwd: Some("/repo"),
                sender_thread_id: None,
                include_side_channel_items: false,
            },
        )
        .unwrap();
        assert_eq!(item["type"], "commandExecution");
        assert_eq!(item["cwd"], "/repo");
        assert_eq!(item["status"], "failed");
        assert_eq!(item["exitCode"], 1);
        assert_eq!(item["durationMs"], 250);
        assert_eq!(item["aggregatedOutput"], "boom");
    }

    #[test]
    fn read_relative_path_resolves_against_context_cwd() {
        let p = part("read", json!({"filePath": "src/lib.rs"}), Some("body"));
        let item = tool_part_to_item_with_context(
            &p,
            ToolPartContext {
                cwd: Some("/repo"),
                sender_thread_id: None,
                include_side_channel_items: false,
            },
        )
        .unwrap();
        assert_eq!(item["cwd"], "/repo");
        assert_eq!(item["commandActions"][0]["path"], "/repo/src/lib.rs");
    }

    #[test]
    fn dynamic_tools_carry_metadata_output_and_success() {
        let part_value = json!({
            "callID": "call-webfetch",
            "tool": "webfetch",
            "state": {
                "status": "completed",
                "input": {"url": "https://example.com", "format": "markdown"},
                "metadata": {"output": "# Example"},
                "time": {"start": 1, "end": 4}
            }
        });
        let item = tool_part_to_item(&part_value).unwrap();
        assert_eq!(item["type"], "dynamicToolCall");
        assert_eq!(item["success"], true);
        assert_eq!(item["durationMs"], 3);
        assert_eq!(item["contentItems"][0]["type"], "inputText");
        assert_eq!(item["contentItems"][0]["text"], "# Example");
    }

    #[test]
    fn mcp_tools_carry_result_or_error() {
        let ok = part(
            "github__list_issues",
            json!({"state": "open"}),
            Some("issues"),
        );
        let ok_item = tool_part_to_item(&ok).unwrap();
        assert_eq!(ok_item["result"]["content"][0]["text"], "issues");

        let failed = json!({
            "callID": "call-mcp",
            "tool": "github__create_issue",
            "state": {
                "status": "error",
                "input": {"title": "x"},
                "error": "bad token"
            }
        });
        let failed_item = tool_part_to_item(&failed).unwrap();
        assert_eq!(failed_item["status"], "failed");
        assert_eq!(failed_item["error"]["message"], "bad token");
    }

    #[test]
    fn task_prefers_real_opencode_session_id() {
        let part_value = json!({
            "callID": "call-task",
            "tool": "task",
            "state": {
                "status": "completed",
                "input": {"prompt": "Inspect", "subagent_type": "general"},
                "metadata": {"sessionId": "ses_child", "summary": "done"}
            }
        });
        let item = tool_part_to_item_with_context(
            &part_value,
            ToolPartContext {
                cwd: None,
                sender_thread_id: Some("thread-parent"),
                include_side_channel_items: false,
            },
        )
        .unwrap();
        assert_eq!(item["receiverThreadIds"][0], "ses_child");
        assert_eq!(item["senderThreadId"], "thread-parent");
        assert_eq!(item["agentsStates"]["ses_child"]["message"], "done");
    }

    #[test]
    fn todowrite_returns_none_emits_via_side_channel() {
        let p = part(
            "todowrite",
            json!({"todos": [
                {"content": "first", "priority": "high", "status": "pending"},
                {"content": "second", "priority": "low", "status": "in_progress"},
                {"content": "third", "priority": "low", "status": "completed"}
            ]}),
            None,
        );
        assert!(tool_part_to_item(&p).is_none(), "no item for todowrite");
        let notifs = tool_part_side_notifications(&p, "th_1", "tu_1");
        assert_eq!(notifs.len(), 1);
        let (method, params) = &notifs[0];
        assert_eq!(*method, "turn/plan/updated");
        assert_eq!(params["threadId"], "th_1");
        assert_eq!(params["turnId"], "tu_1");
        assert_eq!(params["plan"][0]["step"], "first");
        assert_eq!(params["plan"][0]["status"], "pending");
        assert_eq!(params["plan"][1]["status"], "inProgress");
        assert_eq!(params["plan"][2]["status"], "completed");
    }

    #[test]
    fn todowrite_and_question_can_render_for_history() {
        let todo = part(
            "todowrite",
            json!({"todos": [{"content": "first", "status": "pending"}]}),
            None,
        );
        let context = ToolPartContext {
            cwd: None,
            sender_thread_id: None,
            include_side_channel_items: true,
        };
        let todo_item = tool_part_to_item_with_context(&todo, context).unwrap();
        assert_eq!(todo_item["type"], "dynamicToolCall");
        assert_eq!(todo_item["tool"], "todowrite");
        assert_eq!(todo_item["contentItems"][0]["text"], "- [pending] first");

        let question = part(
            "question",
            json!({"questions": [{"header": "Pick", "question": "Choose?"}]}),
            None,
        );
        let question_item = tool_part_to_item_with_context(&question, context).unwrap();
        assert_eq!(question_item["type"], "dynamicToolCall");
        assert_eq!(question_item["tool"], "question");
        assert_eq!(question_item["contentItems"][0]["text"], "Q: Choose?");
    }

    #[test]
    fn question_returns_none() {
        let p = part(
            "question",
            json!({"questions": [{"header": "Pick", "question": "?"}]}),
            None,
        );
        assert!(
            tool_part_to_item(&p).is_none(),
            "question is handled by handle_question_asked, must not double-emit"
        );
    }

    #[test]
    fn unknown_tools_remain_dynamic() {
        for t in ["webfetch", "skill", "novel_tool"] {
            let item = tool_part_to_item(&part(t, json!({}), None)).unwrap();
            assert_eq!(item["type"], "dynamicToolCall", "{t}");
            assert_eq!(item["tool"], t);
        }
    }

    #[test]
    fn mcp_double_underscore_split_unchanged() {
        let item =
            tool_part_to_item(&part("github__create_issue", json!({"title": "x"}), None)).unwrap();
        assert_eq!(item["type"], "mcpToolCall");
        assert_eq!(item["server"], "github");
        assert_eq!(item["tool"], "create_issue");
    }

    #[test]
    fn read_caps_aggregated_output_at_256_kib() {
        let big = "A".repeat(300 * 1024);
        let p = part("read", json!({"filePath": "/big.txt"}), Some(&big));
        let item = tool_part_to_item(&p).unwrap();
        let body = item["aggregatedOutput"].as_str().unwrap();
        assert!(body.len() < 300 * 1024);
        assert!(body.ends_with("[truncated]"));
    }

    #[test]
    fn side_notifications_empty_for_non_todowrite() {
        for t in ["bash", "read", "task", "websearch", "webfetch"] {
            let notifs = tool_part_side_notifications(&part(t, json!({}), None), "t", "u");
            assert!(notifs.is_empty(), "{t} should have no side notifications");
        }
    }
}
