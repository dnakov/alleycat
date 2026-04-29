//! T14 / V9 — `step-finish` parts on `message.part.updated` SSE drive
//! `thread/tokenUsage/updated` notifications.
//!
//! Each step-finish carries `tokens: { total?, input, output, reasoning, cache:{read,write} }`;
//! the bridge accumulates them per thread and emits a snapshot with both the
//! cumulative total and the just-completed step as `last`.

#[path = "support/mod.rs"]
mod support;

use std::time::Duration;

use serde_json::json;
use support::{
    FakeServerState, bring_up_bridge, read_until_notification, read_until_response, send,
};

#[tokio::test]
async fn step_finish_emits_thread_token_usage_updated() {
    let state = std::sync::Arc::new(std::sync::Mutex::new(FakeServerState::default()));
    state.lock().unwrap().route(
        "POST /session?",
        json!({
            "id":"ses_v9",
            "directory":"/tmp/v9",
            "title":"V9",
            "time":{"created":1_000,"updated":1_000}
        }),
    );

    let mut fx = bring_up_bridge("v9", state.clone()).await;

    send(
        &mut fx.write,
        2,
        "thread/start",
        json!({"cwd":"/tmp/v9","model":"openai/gpt-test"}),
    )
    .await;
    let started = read_until_response(&mut fx.read, 2).await;
    let thread_id = started["result"]["thread"]["id"]
        .as_str()
        .expect("thread id")
        .to_string();

    // First step-finish — cumulative totals start at this row's values.
    fx.inject_sse(json!({
        "type": "message.part.updated",
        "properties": {
            "sessionID": "ses_v9",
            "part": {
                "id":"step1","type":"step-finish",
                "reason":"end","cost":0.01,
                "tokens": {
                    "total": 100,
                    "input": 60,
                    "output": 40,
                    "reasoning": 5,
                    "cache": {"read": 10, "write": 0}
                }
            }
        }
    }));
    let first = read_until_notification(
        &mut fx.read,
        "thread/tokenUsage/updated",
        Duration::from_secs(3),
    )
    .await;
    assert_eq!(first["params"]["threadId"], thread_id);
    assert_eq!(first["params"]["tokenUsage"]["total"]["totalTokens"], 100);
    assert_eq!(first["params"]["tokenUsage"]["total"]["inputTokens"], 60);
    assert_eq!(first["params"]["tokenUsage"]["total"]["outputTokens"], 40);
    assert_eq!(
        first["params"]["tokenUsage"]["total"]["cachedInputTokens"],
        10
    );
    assert_eq!(first["params"]["tokenUsage"]["last"]["totalTokens"], 100);

    // Second step-finish — totals accumulate, `last` reflects the just-finished step.
    fx.inject_sse(json!({
        "type": "message.part.updated",
        "properties": {
            "sessionID": "ses_v9",
            "part": {
                "id":"step2","type":"step-finish",
                "reason":"end","cost":0.02,
                "tokens": {
                    "input": 5,
                    "output": 25,
                    "reasoning": 1,
                    "cache": {"read": 2, "write": 0}
                }
            }
        }
    }));
    let second = read_until_notification(
        &mut fx.read,
        "thread/tokenUsage/updated",
        Duration::from_secs(3),
    )
    .await;
    // total derived as input+output when `total` is omitted: 5+25 = 30 added
    // to the prior 100 cumulative = 130.
    assert_eq!(second["params"]["tokenUsage"]["total"]["totalTokens"], 130);
    assert_eq!(second["params"]["tokenUsage"]["total"]["inputTokens"], 65);
    assert_eq!(second["params"]["tokenUsage"]["total"]["outputTokens"], 65);
    assert_eq!(
        second["params"]["tokenUsage"]["total"]["cachedInputTokens"],
        12
    );
    assert_eq!(second["params"]["tokenUsage"]["last"]["totalTokens"], 30);
    assert_eq!(second["params"]["tokenUsage"]["last"]["inputTokens"], 5);

    fx.shutdown().await;
}
