//! T14 / V7 — Tool parts surface as distinct codex item kinds.
//!
//! Verifies the routing in `translate/tool.rs::tool_part_to_item`:
//! - `tool: "bash"` → `commandExecution`
//! - `tool: "write" | "edit" | "patch" | "apply_patch"` → `fileChange`
//! - `tool: "<server>__<name>"` → `mcpToolCall { server, tool: <name> }`
//! - any other tool → `dynamicToolCall { tool }`

#[path = "support/mod.rs"]
mod support;

use serde_json::json;
use support::{FakeServerState, bring_up_bridge, read_until_response, send};

#[tokio::test]
async fn each_tool_kind_routes_to_its_codex_thread_item() {
    let state = std::sync::Arc::new(std::sync::Mutex::new(FakeServerState::default()));
    {
        let mut guard = state.lock().unwrap();
        guard.route(
            "GET /session?directory=%2Ftmp%2Fv7",
            json!([{
                "id":"ses_v7",
                "directory":"/tmp/v7",
                "title":"V7",
                "time":{"created":1_000,"updated":1_000}
            }]),
        );
        guard.route(
            "GET /session/ses_v7/message",
            json!([{
                "info": {"id":"msg","role":"assistant","sessionID":"ses_v7"},
                "parts": [
                    {
                        "id":"p_bash","type":"tool","callID":"call_bash","tool":"bash",
                        "state":{
                            "status":"completed",
                            "input":{"command":"ls","cwd":"/tmp/v7"},
                            "output":"a\nb\n"
                        }
                    },
                    {
                        "id":"p_write","type":"tool","callID":"call_write","tool":"write",
                        "state":{"status":"completed","input":{"path":"/tmp/v7/x","content":"hi"}}
                    },
                    {
                        "id":"p_mcp","type":"tool","callID":"call_mcp","tool":"server_a__do_thing",
                        "state":{"status":"completed","input":{"x":1}}
                    },
                    {
                        "id":"p_dyn","type":"tool","callID":"call_dyn","tool":"custom_tool",
                        "state":{"status":"completed","input":{}}
                    }
                ]
            }]),
        );
    }

    let mut fx = bring_up_bridge("v7", state.clone()).await;

    send(&mut fx.write, 2, "thread/list", json!({"cwd":"/tmp/v7"})).await;
    let list = read_until_response(&mut fx.read, 2).await;
    let thread_id = list["result"]["data"][0]["id"].as_str().unwrap().to_string();

    send(
        &mut fx.write,
        3,
        "thread/read",
        json!({"threadId":thread_id,"includeTurns":true}),
    )
    .await;
    let read = read_until_response(&mut fx.read, 3).await;
    let items = read["result"]["thread"]["turns"][0]["items"]
        .as_array()
        .expect("items array");
    assert_eq!(items.len(), 4);

    assert_eq!(items[0]["type"], "commandExecution");
    assert_eq!(items[0]["command"], "ls");
    assert_eq!(items[0]["cwd"], "/tmp/v7");
    assert_eq!(items[0]["aggregatedOutput"], "a\nb\n");

    assert_eq!(items[1]["type"], "fileChange");

    assert_eq!(items[2]["type"], "mcpToolCall");
    assert_eq!(items[2]["server"], "server_a");
    assert_eq!(items[2]["tool"], "do_thing");

    assert_eq!(items[3]["type"], "dynamicToolCall");
    assert_eq!(items[3]["tool"], "custom_tool");

    fx.shutdown().await;
}
