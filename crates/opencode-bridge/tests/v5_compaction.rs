//! T14 / V5 — `thread/compact/start` round-trip + `session.compacted`
//! notification.
//!
//! 1. `thread/start` → `thread/compact/start` POSTs `/session/{id}/summarize`
//!    with the resolved provider/model. Bridge replies `{}`.
//! 2. Inject SSE `session.compacted{ sessionID }`; bridge translates to a
//!    `thread/compacted` notification keyed to the codex thread id.

#[path = "support/mod.rs"]
mod support;

use std::time::Duration;

use serde_json::json;
use support::{
    FakeServerState, await_captured_body, bring_up_bridge, read_until_notification,
    read_until_response, send,
};

#[tokio::test]
async fn compact_start_calls_summarize_then_session_compacted_emits_thread_compacted() {
    let state = std::sync::Arc::new(std::sync::Mutex::new(FakeServerState::default()));
    {
        let mut guard = state.lock().unwrap();
        guard.route(
            "POST /session?",
            json!({
                "id":"ses_v5",
                "directory":"/tmp/v5",
                "title":"V5",
                "time":{"created":1_000,"updated":1_000}
            }),
        );
        // The summarize body returns boolean true per opencode's contract;
        // the bridge ignores the body and just acks.
        guard.route("POST /session/ses_v5/summarize", json!(true));
    }

    let mut fx = bring_up_bridge("v5", state.clone()).await;

    // thread/start → bind ses_v5.
    send(
        &mut fx.write,
        2,
        "thread/start",
        json!({"cwd":"/tmp/v5","model":"openai/gpt-test"}),
    )
    .await;
    let started = read_until_response(&mut fx.read, 2).await;
    let thread_id = started["result"]["thread"]["id"]
        .as_str()
        .expect("thread id")
        .to_string();

    // thread/compact/start → POST /session/ses_v5/summarize { providerID, modelID, auto:false }.
    send(
        &mut fx.write,
        3,
        "thread/compact/start",
        json!({"threadId":thread_id,"model":"openai/gpt-test"}),
    )
    .await;
    let compact_resp = read_until_response(&mut fx.read, 3).await;
    assert!(compact_resp["result"].is_object(), "expected `{{}}` ack: {compact_resp:?}");

    // Confirm summarize was called with the right payload.
    let body = await_captured_body(
        &fx.state,
        "POST /session/ses_v5/summarize",
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(body["providerID"], "openai");
    assert_eq!(body["modelID"], "gpt-test");
    assert_eq!(body["auto"], false);

    // Now emit `session.compacted` over SSE; expect a `thread/compacted`
    // notification keyed to the codex thread id.
    fx.inject_sse(json!({
        "type": "session.compacted",
        "properties": { "sessionID": "ses_v5" }
    }));
    let notif = read_until_notification(&mut fx.read, "thread/compacted", Duration::from_secs(3)).await;
    assert_eq!(notif["params"]["threadId"], thread_id);

    fx.shutdown().await;
}

#[tokio::test]
async fn compact_start_with_no_explicit_model_uses_config_providers_default() {
    let state = std::sync::Arc::new(std::sync::Mutex::new(FakeServerState::default()));
    {
        let mut guard = state.lock().unwrap();
        guard.route(
            "POST /session?",
            json!({
                "id":"ses_v5b",
                "directory":"/tmp/v5",
                "title":"V5B",
                "time":{"created":1_000,"updated":1_000}
            }),
        );
        guard.route(
            "GET /config/providers",
            json!({"providers":[],"default":{"anthropic":"claude-sonnet"}}),
        );
        guard.route("POST /session/ses_v5b/summarize", json!(true));
    }
    let mut fx = bring_up_bridge("v5b", state.clone()).await;

    send(
        &mut fx.write,
        2,
        "thread/start",
        json!({"cwd":"/tmp/v5"}),
    )
    .await;
    let started = read_until_response(&mut fx.read, 2).await;
    let thread_id = started["result"]["thread"]["id"]
        .as_str()
        .expect("thread id")
        .to_string();

    // No `model` field in params — bridge should fall back to the configured
    // default from `/config/providers`.
    send(
        &mut fx.write,
        3,
        "thread/compact/start",
        json!({"threadId":thread_id}),
    )
    .await;
    let _ = read_until_response(&mut fx.read, 3).await;

    let body = await_captured_body(
        &fx.state,
        "POST /session/ses_v5b/summarize",
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(body["providerID"], "anthropic");
    assert_eq!(body["modelID"], "claude-sonnet");

    fx.shutdown().await;
}
