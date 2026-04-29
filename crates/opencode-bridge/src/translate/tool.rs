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
//! | `websearch`                          | `webSearch`                                      |
//! | `task`                               | `collabAgentToolCall`                            |
//! | `todowrite`, `question`              | no item (side-channel notification carries them) |
//! | anything else (`webfetch`, `codesearch`, `skill`, ...) | `dynamicToolCall`              |
//!
//! `todowrite` and `question` return `None` from this function because their
//! payloads ride a different codex channel:
//! - `todowrite` → emit `turn/plan/updated` via [`tool_part_side_notifications`].
//! - `question` → live SSE handler `handle_question_asked` dispatches
//!   `item/tool/requestUserInput`. Suppressing the dynamic-tool item here
//!   avoids a duplicate card.

use serde_json::{Value, json};

/// Hard cap on `aggregatedOutput` for read/grep/glob results so a multi-MB
/// file body doesn't bloat every notification round-trip.
const EXPLORATION_OUTPUT_CAP: usize = 256 * 1024;

pub fn tool_part_to_item(part: &Value) -> Option<Value> {
    let id = part
        .get("callID")
        .or_else(|| part.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let tool = part.get("tool").and_then(Value::as_str).unwrap_or("tool");
    let state = part.get("state").cloned().unwrap_or(Value::Null);
    let status = state
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("completed")
        .to_string();
    let input = state.get("input").cloned().unwrap_or(Value::Null);

    if tool == "bash" {
        return Some(json!({
            "type": "commandExecution",
            "id": id,
            "command": input.get("command").and_then(Value::as_str).unwrap_or(""),
            "cwd": input.get("cwd").and_then(Value::as_str),
            "status": status,
            "aggregatedOutput": state.get("output").and_then(Value::as_str).unwrap_or(""),
        }));
    }
    if matches!(tool, "write" | "edit" | "patch" | "apply_patch") {
        return Some(json!({
            "type": "fileChange",
            "id": id,
            "changes": synthesize_file_changes(tool, &state, &input),
            "status": status,
        }));
    }
    if let Some((server, name)) = tool.split_once("__") {
        return Some(json!({
            "type": "mcpToolCall",
            "id": id,
            "server": server,
            "tool": name,
            "arguments": input,
            "status": status,
        }));
    }

    match tool {
        "read" => {
            let path = input
                .get("filePath")
                .or_else(|| input.get("path"))
                .and_then(Value::as_str)
                .unwrap_or("");
            Some(json!({
                "type": "commandExecution",
                "id": id,
                "command": format!("read {path}"),
                "status": status,
                "commandActions": [{"type": "read", "path": path}],
                "aggregatedOutput": cap_output(extract_text_output(&state)),
            }))
        }
        "glob" => {
            let path = input.get("path").and_then(Value::as_str);
            let pattern = input.get("pattern").and_then(Value::as_str);
            let display = match (pattern, path) {
                (Some(p), Some(d)) => format!("glob {p} {d}"),
                (Some(p), None) => format!("glob {p}"),
                (None, Some(d)) => format!("glob {d}"),
                _ => "glob".to_string(),
            };
            let mut action = json!({"type": "list_files"});
            if let Some(p) = path {
                action["path"] = Value::String(p.to_string());
            }
            if let Some(p) = pattern {
                action["pattern"] = Value::String(p.to_string());
            }
            Some(json!({
                "type": "commandExecution",
                "id": id,
                "command": display,
                "status": status,
                "commandActions": [action],
                "aggregatedOutput": cap_output(extract_text_output(&state)),
            }))
        }
        "grep" => {
            let pattern = input.get("pattern").and_then(Value::as_str).unwrap_or("");
            let path = input.get("path").and_then(Value::as_str);
            let mut action = json!({"type": "search", "query": pattern});
            if let Some(p) = path {
                action["path"] = Value::String(p.to_string());
            }
            Some(json!({
                "type": "commandExecution",
                "id": id,
                "command": format!("grep {pattern}"),
                "status": status,
                "commandActions": [action],
                "aggregatedOutput": cap_output(extract_text_output(&state)),
            }))
        }
        "websearch" => {
            let query = input.get("query").and_then(Value::as_str).unwrap_or("");
            Some(json!({
                "type": "webSearch",
                "id": id,
                "query": query,
                "action": {"type": "search"},
            }))
        }
        "task" => {
            let prompt = input.get("prompt").and_then(Value::as_str);
            let subagent_type = input.get("subagent_type").and_then(Value::as_str);
            let description = input.get("description").and_then(Value::as_str);
            let label = match (subagent_type, description) {
                (Some(t), Some(d)) if !t.is_empty() && !d.is_empty() => Some(format!("{t}: {d}")),
                (Some(t), _) if !t.is_empty() => Some(t.to_string()),
                (_, Some(d)) if !d.is_empty() => Some(d.to_string()),
                _ => None,
            };
            let receiver_id = format!("subagent-{id}");
            let mut agents_states = serde_json::Map::new();
            let mut state_obj = json!({"status": "completed"});
            if let Some(label) = label {
                state_obj["message"] = Value::String(label);
            }
            agents_states.insert(receiver_id.clone(), state_obj);
            let collab_status = if status == "error" {
                "failed"
            } else {
                "completed"
            };
            let mut item = json!({
                "type": "collabAgentToolCall",
                "id": id,
                "tool": "spawnAgent",
                "status": collab_status,
                "senderThreadId": "",
                "receiverThreadIds": [receiver_id],
                "agentsStates": agents_states,
            });
            if let Some(p) = prompt {
                item["prompt"] = Value::String(p.to_string());
            }
            Some(item)
        }
        // No-item kinds: their payloads ride side-channel notifications.
        "todowrite" | "question" => None,
        _ => Some(json!({
            "type": "dynamicToolCall",
            "id": id,
            "tool": tool,
            "arguments": input,
            "status": status,
        })),
    }
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
        let item =
            tool_part_to_item(&part("write", json!({"content": "x"}), None)).unwrap();
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
        assert_eq!(item["commandActions"][0]["path"], "/tmp/x");
        assert_eq!(item["aggregatedOutput"], "hello");
    }

    #[test]
    fn read_falls_back_to_path_when_filePath_missing() {
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
        assert_eq!(item["commandActions"][0]["type"], "list_files");
        assert_eq!(item["commandActions"][0]["pattern"], "**/*.md");
        assert_eq!(item["commandActions"][0]["path"], "/repo");
    }

    #[test]
    fn grep_emits_search_action() {
        let p = part("grep", json!({"pattern": "TODO", "path": "src"}), None);
        let item = tool_part_to_item(&p).unwrap();
        assert_eq!(item["type"], "commandExecution");
        assert_eq!(item["command"], "grep TODO");
        assert_eq!(item["commandActions"][0]["type"], "search");
        assert_eq!(item["commandActions"][0]["query"], "TODO");
        assert_eq!(item["commandActions"][0]["path"], "src");
    }

    #[test]
    fn websearch_emits_web_search_item() {
        let p = part("websearch", json!({"query": "rust async"}), None);
        let item = tool_part_to_item(&p).unwrap();
        assert_eq!(item["type"], "webSearch");
        assert_eq!(item["query"], "rust async");
        assert_eq!(item["action"]["type"], "search");
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
        for t in ["webfetch", "codesearch", "skill", "novel_tool"] {
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
