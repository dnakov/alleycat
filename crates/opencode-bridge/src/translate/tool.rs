use serde_json::{Value, json};

pub fn tool_part_to_item(part: &Value) -> Option<Value> {
    let id = part
        .get("callID")
        .or_else(|| part.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let tool = part.get("tool").and_then(Value::as_str).unwrap_or("tool");
    let state = part.get("state").cloned().unwrap_or(Value::Null);
    if tool == "bash" {
        return Some(json!({
            "type":"commandExecution",
            "id":id,
            "command": state.pointer("/input/command").and_then(Value::as_str).unwrap_or(""),
            "cwd": state.pointer("/input/cwd").and_then(Value::as_str),
            "status": state.get("status").and_then(Value::as_str).unwrap_or("completed"),
            "aggregatedOutput": state.get("output").and_then(Value::as_str).unwrap_or("")
        }));
    }
    if matches!(tool, "write" | "edit" | "patch" | "apply_patch") {
        return Some(
            json!({"type":"fileChange","id":id,"changes":[],"status":state.get("status").and_then(Value::as_str).unwrap_or("completed")}),
        );
    }
    if let Some((server, name)) = tool.split_once("__") {
        return Some(
            json!({"type":"mcpToolCall","id":id,"server":server,"tool":name,"arguments":state.get("input").cloned().unwrap_or(Value::Null),"status":state.get("status").and_then(Value::as_str).unwrap_or("completed")}),
        );
    }
    Some(
        json!({"type":"dynamicToolCall","id":id,"tool":tool,"arguments":state.get("input").cloned().unwrap_or(Value::Null),"status":state.get("status").and_then(Value::as_str).unwrap_or("completed")}),
    )
}
