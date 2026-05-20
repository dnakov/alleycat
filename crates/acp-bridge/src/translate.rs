//! Translation between Codex protocol and ACP protocol.

use serde_json::Value;

/// Translate Codex InitializeParams to ACP InitializeRequest.
pub fn codex_to_acp_initialize(codex_params: &Value) -> Result<Value, anyhow::Error> {
    let acp_request = serde_json::json!({
        "protocolVersion": "1.0.0",
        "clientCapabilities": {
            "fs": {
                "readTextFile": true,
                "writeTextFile": true,
            },
            "terminal": true,
        },
        "clientInfo": {
            "name": codex_params.get("clientInfo")
                .and_then(|v| v.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("Alleycat"),
            "version": codex_params.get("clientInfo")
                .and_then(|v| v.get("version"))
                .and_then(|v| v.as_str())
                .unwrap_or("0.1.0"),
        },
    });
    Ok(acp_request)
}

/// Translate ACP InitializeResponse to Codex InitializeResult.
pub fn acp_to_codex_initialize_result(acp_response: &Value) -> Result<Value, anyhow::Error> {
    let agent_name = acp_response
        .get("agentInfo")
        .and_then(|v| v.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("ACP Agent");
    let agent_version = acp_response
        .get("agentInfo")
        .and_then(|v| v.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or("1.0.0");
    let codex_home = std::env::var("HOME")
        .map(|home| format!("{home}/.alleycat-acp-bridge"))
        .unwrap_or_else(|_| "/tmp/alleycat-acp-bridge".to_string());
    let codex_result = serde_json::json!({
        "userAgent": format!("alleycat-acp-bridge/{} ({agent_name} {agent_version})", env!("CARGO_PKG_VERSION")),
        "codexHome": codex_home,
        "platformFamily": std::env::consts::FAMILY,
        "platformOs": std::env::consts::OS,
    });
    Ok(codex_result)
}

/// Translate Codex ThreadStartParams to ACP NewSessionRequest.
///
/// Per ACP spec (https://agentclientprotocol.com/protocol/session-setup),
/// `session/new` params require both `cwd: string` and `mcpServers: array`
/// (even when empty). Devin's serde rejects requests missing
/// `mcpServers` with `"missing field mcpServers"`.
pub fn codex_to_acp_new_session(codex_params: &Value) -> Result<Value, anyhow::Error> {
    // Grok rejects relative paths (including the empty string) with
    // `-32602 Invalid params: Path is not absolute`, so fall through to
    // `/` whenever the client didn't supply a valid absolute cwd.
    let cwd = codex_params
        .get("cwd")
        .and_then(|v| v.as_str())
        .filter(|s| s.starts_with('/'))
        .unwrap_or("/");

    let acp_request = serde_json::json!({
        "cwd": cwd,
        "mcpServers": [],
    });
    Ok(acp_request)
}

/// Translate ACP NewSessionResponse to Codex ThreadStartResponse.
pub fn acp_to_codex_thread_start(acp_response: &Value) -> Result<Value, anyhow::Error> {
    let session_id = acp_response
        .get("sessionId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing sessionId in ACP response"))?;

    let codex_response = serde_json::json!({
        "threadId": session_id,
        "title": format!("Session {}", session_id),
        "agentNickname": null,
        "agentRole": null,
    });
    Ok(codex_response)
}

/// Translate Codex message to ACP PromptRequest.
pub fn codex_to_acp_prompt(codex_message: &Value) -> Result<Value, anyhow::Error> {
    let content = codex_message
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let acp_request = serde_json::json!({
        "sessionId": codex_message.get("threadId"),
        "messages": [{
            "role": "user",
            "content": [{
                "type": "text",
                "text": content,
            }],
        }],
    });
    Ok(acp_request)
}

/// Translate Codex tool call to ACP file operation if applicable.
/// Returns Some(ACP request) if the tool maps to an ACP fs operation, None otherwise.
pub fn codex_tool_to_acp_fs_operation(
    tool_name: &str,
    tool_input: &Value,
    session_id: &str,
) -> Option<Value> {
    match tool_name {
        "Read" | "read" => {
            let path = tool_input
                .get("file_path")
                .or_else(|| tool_input.get("path"))
                .and_then(|v| v.as_str())?;

            Some(serde_json::json!({
                "sessionId": session_id,
                "path": path,
            }))
        }
        "Write" | "write" => {
            let path = tool_input
                .get("file_path")
                .or_else(|| tool_input.get("path"))
                .and_then(|v| v.as_str())?;
            let content = tool_input.get("content").and_then(|v| v.as_str())?;

            Some(serde_json::json!({
                "sessionId": session_id,
                "path": path,
                "content": content,
            }))
        }
        "FileExists" | "file_exists" => {
            let path = tool_input
                .get("file_path")
                .or_else(|| tool_input.get("path"))
                .and_then(|v| v.as_str())?;

            Some(serde_json::json!({
                "sessionId": session_id,
                "path": path,
            }))
        }
        "ListDirectory" | "list_directory" | "ListFiles" | "list_files" => {
            let path = tool_input
                .get("file_path")
                .or_else(|| tool_input.get("path"))
                .and_then(|v| v.as_str())?;

            Some(serde_json::json!({
                "sessionId": session_id,
                "path": path,
            }))
        }
        "CreateDirectory" | "create_directory" => {
            let path = tool_input
                .get("file_path")
                .or_else(|| tool_input.get("path"))
                .and_then(|v| v.as_str())?;

            Some(serde_json::json!({
                "sessionId": session_id,
                "path": path,
            }))
        }
        "DeleteFile" | "delete_file" => {
            let path = tool_input
                .get("file_path")
                .or_else(|| tool_input.get("path"))
                .and_then(|v| v.as_str())?;

            Some(serde_json::json!({
                "sessionId": session_id,
                "path": path,
            }))
        }
        _ => None, // Other tools don't map to ACP fs operations
    }
}

/// Translate ACP file operation result to Codex tool result.
pub fn acp_fs_result_to_codex_tool_result(
    fs_operation: &str,
    acp_result: &Value,
) -> Result<Value, anyhow::Error> {
    match fs_operation {
        "fs/read_text_file" => {
            let content = acp_result.as_str().unwrap_or("");
            Ok(serde_json::json!({
                "content": content,
            }))
        }
        "fs/write_text_file" => Ok(serde_json::json!({
            "success": true,
        })),
        "fs/file_exists" => {
            let exists = acp_result.as_bool().unwrap_or(false);
            Ok(serde_json::json!({
                "exists": exists,
            }))
        }
        "fs/list_directory" => {
            // ACP might return a list of files/directories
            Ok(serde_json::json!({
                "entries": acp_result,
            }))
        }
        "fs/create_directory" => Ok(serde_json::json!({
            "success": true,
        })),
        "fs/delete_file" => Ok(serde_json::json!({
            "success": true,
        })),
        _ => Ok(serde_json::json!({})),
    }
}
