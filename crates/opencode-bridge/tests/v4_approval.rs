//! T6 acceptance: opencode `permission.asked` SSE → bridge sends a
//! server→client `item/commandExecution/requestApproval` (or
//! `item/fileChange/requestApproval`) → on the codex client's response
//! → bridge POSTs `/permission/{requestID}/reply` with the bucketed reply.
//!
//! Two cases:
//!
//! 1. Command-shaped permission (`metadata.command` set) → command approval,
//!    decision `"denied"` → reply `reject`.
//! 2. File-shaped permission (no `metadata.command`, `permission:"write"`) →
//!    file-change approval, decision `"acceptForSession"` → reply `always`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

#[path = "support/mod.rs"]
mod support;

use support::{
    FakeServerState, await_captured_body, bring_up_bridge, read_until_server_request,
    send_server_response,
};

#[tokio::test]
async fn permission_asked_bash_routes_to_command_approval_and_replies_reject() {
    let state = Arc::new(Mutex::new(FakeServerState::default()));
    {
        let mut st = state.lock().unwrap();
        st.route(
            "POST /session?",
            json!({
                "id":"ses_1",
                "directory":"/tmp/opencode-v4",
                "title":"V4",
                "time":{"created":1000,"updated":1000}
            }),
        );
        st.route(
            "GET /provider",
            json!({"all":[],"default":[],"connected":[]}),
        );
        // Capture-only route — body is what we assert on.
        st.route("POST /permission/perm_1/reply", json!({}));
    }
    let mut fx = bring_up_bridge("v4", Arc::clone(&state)).await;
    let thread_id = fx.start_thread("/tmp/opencode-v4").await;

    // Inject the opencode `permission.asked` event.
    fx.inject_sse(json!({
        "type": "permission.asked",
        "properties": {
            "id": "perm_1",
            "sessionID": "ses_1",
            "permission": "bash",
            "patterns": [],
            "metadata": {
                "command": "rm -rf /",
                "cwd": "/tmp"
            },
            "always": []
        }
    }));

    // The bridge should send `item/commandExecution/requestApproval` on the
    // codex socket. Pluck the bridge-generated request id.
    let request = read_until_server_request(
        &mut fx.read,
        "item/commandExecution/requestApproval",
        Duration::from_secs(5),
    )
    .await;
    let request_id_value = request["id"].clone();
    let params = &request["params"];
    assert_eq!(params["threadId"], thread_id);
    assert_eq!(params["itemId"], "perm_1");
    assert_eq!(params["command"], "rm -rf /");
    assert_eq!(params["cwd"], "/tmp");

    // Echo a `denied` decision back.
    send_server_response(
        &mut fx.write,
        &request_id_value,
        json!({"decision": "denied"}),
    )
    .await;

    // The bridge should now POST `/permission/perm_1/reply` upstream.
    let captured = await_captured_body(
        &fx.state,
        "POST /permission/perm_1/reply",
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(captured["reply"], "reject");

    fx.shutdown().await;
}

#[tokio::test]
async fn permission_asked_write_routes_to_file_change_approval_and_replies_always() {
    let state = Arc::new(Mutex::new(FakeServerState::default()));
    {
        let mut st = state.lock().unwrap();
        st.route(
            "POST /session?",
            json!({
                "id":"ses_1",
                "directory":"/tmp/opencode-v4-fs",
                "title":"V4-FS",
                "time":{"created":1000,"updated":1000}
            }),
        );
        st.route(
            "GET /provider",
            json!({"all":[],"default":[],"connected":[]}),
        );
        st.route("POST /permission/perm_2/reply", json!({}));
    }
    let mut fx = bring_up_bridge("v4-fs", Arc::clone(&state)).await;
    let thread_id = fx.start_thread("/tmp/opencode-v4-fs").await;

    fx.inject_sse(json!({
        "type": "permission.asked",
        "properties": {
            "id": "perm_2",
            "sessionID": "ses_1",
            "permission": "write",
            "patterns": [],
            "metadata": {
                "path": "/tmp/secret.txt"
            },
            "always": []
        }
    }));

    let request = read_until_server_request(
        &mut fx.read,
        "item/fileChange/requestApproval",
        Duration::from_secs(5),
    )
    .await;
    let request_id_value = request["id"].clone();
    let params = &request["params"];
    assert_eq!(params["threadId"], thread_id);
    assert_eq!(params["itemId"], "perm_2");

    // `acceptForSession` buckets to `ApprovedForSession` → opencode reply
    // `always`.
    send_server_response(
        &mut fx.write,
        &request_id_value,
        json!({"decision": "acceptForSession"}),
    )
    .await;

    let captured = await_captured_body(
        &fx.state,
        "POST /permission/perm_2/reply",
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(captured["reply"], "always");
    let _ = thread_id;

    fx.shutdown().await;
}

#[tokio::test]
async fn permission_asked_accept_replies_once() {
    let state = Arc::new(Mutex::new(FakeServerState::default()));
    {
        let mut st = state.lock().unwrap();
        st.route(
            "POST /session?",
            json!({
                "id":"ses_1",
                "directory":"/tmp/opencode-v4-once",
                "title":"V4-Once",
                "time":{"created":1000,"updated":1000}
            }),
        );
        st.route(
            "GET /provider",
            json!({"all":[],"default":[],"connected":[]}),
        );
        st.route("POST /permission/perm_3/reply", json!({}));
    }
    let mut fx = bring_up_bridge("v4-once", Arc::clone(&state)).await;
    let _thread_id = fx.start_thread("/tmp/opencode-v4-once").await;

    fx.inject_sse(json!({
        "type": "permission.asked",
        "properties": {
            "id": "perm_3",
            "sessionID": "ses_1",
            "permission": "bash",
            "patterns": [],
            "metadata": {"command": "ls"},
            "always": []
        }
    }));

    let request = read_until_server_request(
        &mut fx.read,
        "item/commandExecution/requestApproval",
        Duration::from_secs(5),
    )
    .await;
    let id: Value = request["id"].clone();
    send_server_response(&mut fx.write, &id, json!({"decision": "accept"})).await;

    let captured = await_captured_body(
        &fx.state,
        "POST /permission/perm_3/reply",
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(captured["reply"], "once");

    fx.shutdown().await;
}
