//! T7 acceptance: opencode `question.asked` SSE → bridge sends a
//! server→client `item/tool/requestUserInput` → on the codex client's
//! response (a `ToolRequestUserInputResponse` shaped as
//! `{ answers: { id: { answers: [...] } } }`) the bridge POSTs
//! `/question/{requestID}/reply` with opencode's `Reply.answers:
//! Array<Array<string>>` ordered by the original question index.
//!
//! The test deliberately uses **out-of-order** keys in the response (`"1"`
//! before `"0"`) to exercise the `answers_in_question_order` reordering in
//! `translate/events.rs`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

#[path = "support/mod.rs"]
mod support;

use support::{
    FakeServerState, await_captured_body, bring_up_bridge,
    read_until_server_request, send_server_response,
};

#[tokio::test]
async fn question_asked_two_questions_round_trip() {
    let state = Arc::new(Mutex::new(FakeServerState::default()));
    {
        let mut st = state.lock().unwrap();
        st.route(
            "POST /session?",
            json!({
                "id":"ses_1",
                "directory":"/tmp/opencode-v6",
                "title":"V6",
                "time":{"created":1000,"updated":1000}
            }),
        );
        st.route("GET /provider", json!({"all":[],"default":[],"connected":[]}));
        st.route("POST /question/q_1/reply", json!({}));
    }
    let mut fx = bring_up_bridge("v6", Arc::clone(&state)).await;
    let thread_id = fx.start_thread("/tmp/opencode-v6").await;

    fx.inject_sse(json!({
        "type": "question.asked",
        "properties": {
            "id": "q_1",
            "sessionID": "ses_1",
            "questions": [
                {
                    "header": "H1",
                    "question": "Q1",
                    "options": [
                        {"label":"yes","description":""},
                        {"label":"no","description":""}
                    ]
                },
                {
                    "header": "H2",
                    "question": "Q2",
                    "options": [
                        {"label":"a","description":""},
                        {"label":"b","description":""}
                    ]
                }
            ]
        }
    }));

    let request = read_until_server_request(
        &mut fx.read,
        "item/tool/requestUserInput",
        Duration::from_secs(5),
    )
    .await;
    let id_value = request["id"].clone();
    let params = &request["params"];
    assert_eq!(params["threadId"], thread_id);
    assert_eq!(params["itemId"], "q_1");
    let questions = params["questions"]
        .as_array()
        .expect("questions array on requestUserInput params");
    assert_eq!(questions.len(), 2);
    assert_eq!(questions[0]["id"], "0");
    assert_eq!(questions[1]["id"], "1");
    assert_eq!(questions[0]["question"], "Q1");
    assert_eq!(questions[1]["question"], "Q2");

    // Out-of-order keys in the response — the bridge must reorder by index
    // to produce opencode's flat `[["yes"], ["a","b"]]`.
    send_server_response(
        &mut fx.write,
        &id_value,
        json!({
            "answers": {
                "1": {"answers": ["a", "b"]},
                "0": {"answers": ["yes"]}
            }
        }),
    )
    .await;

    let captured = await_captured_body(
        &fx.state,
        "POST /question/q_1/reply",
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(
        captured["answers"],
        json!([["yes"], ["a", "b"]]),
        "answers must be reordered by original question index"
    );

    fx.shutdown().await;
}

#[tokio::test]
async fn question_asked_missing_answer_pads_with_empty_array() {
    let state = Arc::new(Mutex::new(FakeServerState::default()));
    {
        let mut st = state.lock().unwrap();
        st.route(
            "POST /session?",
            json!({
                "id":"ses_1",
                "directory":"/tmp/opencode-v6-pad",
                "title":"V6-Pad",
                "time":{"created":1000,"updated":1000}
            }),
        );
        st.route("GET /provider", json!({"all":[],"default":[],"connected":[]}));
        st.route("POST /question/q_2/reply", json!({}));
    }
    let mut fx = bring_up_bridge("v6-pad", Arc::clone(&state)).await;
    let _thread_id = fx.start_thread("/tmp/opencode-v6-pad").await;

    fx.inject_sse(json!({
        "type": "question.asked",
        "properties": {
            "id": "q_2",
            "sessionID": "ses_1",
            "questions": [
                {"header":"H1","question":"Q1","options":[{"label":"x","description":""}]},
                {"header":"H2","question":"Q2","options":[{"label":"y","description":""}]},
                {"header":"H3","question":"Q3","options":[{"label":"z","description":""}]}
            ]
        }
    }));

    let request = read_until_server_request(
        &mut fx.read,
        "item/tool/requestUserInput",
        Duration::from_secs(5),
    )
    .await;
    let id_value = request["id"].clone();

    // Only answer the middle question — bridge must pad indices 0 and 2 with
    // empty arrays per `answers_in_question_order`.
    send_server_response(
        &mut fx.write,
        &id_value,
        json!({"answers": {"1": {"answers": ["y"]}}}),
    )
    .await;

    let captured = await_captured_body(
        &fx.state,
        "POST /question/q_2/reply",
        Duration::from_secs(5),
    )
    .await;
    let answers = captured["answers"].as_array().expect("answers array");
    assert_eq!(answers.len(), 3);
    assert_eq!(answers[0], json!([]));
    assert_eq!(answers[1], json!(["y"]));
    assert_eq!(answers[2], json!([]));

    fx.shutdown().await;
}
