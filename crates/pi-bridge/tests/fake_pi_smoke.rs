//! Smoke test for the `fake-pi` test harness binary.
//!
//! These tests do not exercise codex-side behavior — they just confirm that
//! the fake satisfies pi's wire contract well enough for `PiProcessHandle` to
//! drive it. Failures here indicate the fake drifted from pi's RPC shape, not
//! that the bridge is broken.

mod support;

use std::time::Duration;

use alleycat_pi_bridge::pool::pi_protocol::{
    BareCmd, NewSessionCmd, PiEvent, PromptCmd, RpcCommand,
};
use alleycat_pi_bridge::pool::process::PiProcessHandle;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::timeout;

use support::{fake_pi_path, write_script};

#[tokio::test]
async fn fake_pi_acks_new_session_and_get_state() {
    let cwd = TempDir::new().unwrap();
    let handle = PiProcessHandle::spawn(cwd.path(), fake_pi_path())
        .await
        .expect("spawn fake-pi");

    let response = handle
        .send_request(RpcCommand::NewSession(NewSessionCmd {
            id: None,
            parent_session: None,
        }))
        .await
        .expect("new_session");
    assert!(response.success, "new_session should succeed");
    assert_eq!(response.command, "new_session");

    let state = handle
        .send_request(RpcCommand::GetState(BareCmd { id: None }))
        .await
        .expect("get_state");
    assert!(state.success);
    let data = state.data.expect("get_state response should carry data");
    let session_id = data
        .get("sessionId")
        .and_then(|v| v.as_str())
        .expect("sessionId in get_state data");
    assert!(!session_id.is_empty(), "session_id was assigned");

    handle.shutdown().await;
}

#[tokio::test]
async fn fake_pi_emits_scripted_events_on_prompt() {
    let cwd = TempDir::new().unwrap();
    let script_dir = TempDir::new().unwrap();
    let script_path = write_script(
        script_dir.path(),
        &[
            json!({"type": "agent_start"}),
            json!({"type": "turn_start"}),
            json!({"type": "agent_end", "messages": []}),
        ],
    );

    // Inject the script before spawn so the child inherits the env var.
    // Safety: this test does not run in parallel with anything else that
    // reads `FAKE_PI_SCRIPT` (it's process-global env).
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

    let response = handle
        .send_request(RpcCommand::Prompt(PromptCmd {
            id: None,
            message: "hello".to_string(),
            images: Vec::new(),
            streaming_behavior: None,
        }))
        .await
        .expect("prompt");
    assert!(response.success);

    // Drain the broadcast channel until we see agent_end. We bound the wait
    // so a regression in the fake shows up as a timeout, not a hang.
    let mut saw_start = false;
    let mut saw_turn = false;
    let mut saw_end = false;
    for _ in 0..10 {
        let evt = timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("event before timeout")
            .expect("broadcast not closed");
        match evt {
            PiEvent::AgentStart => saw_start = true,
            PiEvent::TurnStart => saw_turn = true,
            PiEvent::AgentEnd { .. } => {
                saw_end = true;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_start, "agent_start fired");
    assert!(saw_turn, "turn_start fired");
    assert!(saw_end, "agent_end fired");

    handle.shutdown().await;
}
