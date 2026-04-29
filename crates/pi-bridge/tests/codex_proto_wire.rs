//! Wire-format smoke tests for `codex_proto`.
//!
//! These exercise the JSON shapes the bridge must emit/accept. The `expected`
//! JSON values were taken from codex's own
//! `app-server-protocol/src/protocol/common.rs` test cases (e.g.
//! `serialize_initialize_with_opt_out_notification_methods`,
//! `serialize_thread_status_changed_notification`,
//! `account_serializes_fields_in_camel_case`). If these tests start failing,
//! either codex changed the wire format upstream or our mirror drifted.

use alleycat_pi_bridge::codex_proto as p;
use serde_json::json;

#[test]
fn initialize_params_round_trip() {
    let params = p::InitializeParams {
        client_info: p::ClientInfo {
            name: "codex_vscode".to_string(),
            title: Some("Codex VS Code Extension".to_string()),
            version: "0.1.0".to_string(),
        },
        capabilities: Some(p::InitializeCapabilities {
            experimental_api: true,
            opt_out_notification_methods: Some(vec![
                "thread/started".to_string(),
                "item/agentMessage/delta".to_string(),
            ]),
        }),
    };

    let v = serde_json::to_value(&params).unwrap();
    assert_eq!(
        v,
        json!({
            "clientInfo": {
                "name": "codex_vscode",
                "title": "Codex VS Code Extension",
                "version": "0.1.0",
            },
            "capabilities": {
                "experimentalApi": true,
                "optOutNotificationMethods": [
                    "thread/started",
                    "item/agentMessage/delta",
                ],
            },
        }),
    );

    let back: p::InitializeParams = serde_json::from_value(v).unwrap();
    assert_eq!(back, params);
}

#[test]
fn thread_status_changed_serializes_with_tagged_status() {
    let n = p::ServerNotification::ThreadStatusChanged(p::ThreadStatusChangedNotification {
        thread_id: "thr_123".to_string(),
        status: p::ThreadStatus::Idle,
    });
    assert_eq!(
        serde_json::to_value(&n).unwrap(),
        json!({
            "method": "thread/status/changed",
            "params": {
                "threadId": "thr_123",
                "status": { "type": "idle" },
            },
        }),
    );
}

#[test]
fn user_input_text_uses_type_tag() {
    let v = serde_json::to_value(&p::UserInput::Text {
        text: "hello".to_string(),
        text_elements: Vec::new(),
    })
    .unwrap();
    assert_eq!(
        v,
        json!({
            "type": "text",
            "text": "hello",
            "textElements": [],
        }),
    );
}

#[test]
fn agent_message_item_round_trip() {
    let item = p::ThreadItem::AgentMessage {
        id: "msg_1".to_string(),
        text: "hi".to_string(),
        phase: None,
        memory_citation: None,
    };
    let v = serde_json::to_value(&item).unwrap();
    assert_eq!(
        v,
        json!({
            "type": "agentMessage",
            "id": "msg_1",
            "text": "hi",
        }),
    );
    let back: p::ThreadItem = serde_json::from_value(v).unwrap();
    assert_eq!(back, item);
}

#[test]
fn account_chatgpt_round_trips_camel_case_plan_type() {
    let acc = p::Account::Chatgpt {
        email: "user@example.com".to_string(),
        plan_type: json!("plus"),
    };
    assert_eq!(
        serde_json::to_value(&acc).unwrap(),
        json!({
            "type": "chatgpt",
            "email": "user@example.com",
            "planType": "plus",
        }),
    );
}

#[test]
fn login_account_chatgpt_auth_tokens_camel_case() {
    let p = p::LoginAccountParams::ChatgptAuthTokens {
        access_token: "access-token".to_string(),
        chatgpt_account_id: "org-123".to_string(),
        chatgpt_plan_type: Some("business".to_string()),
    };
    assert_eq!(
        serde_json::to_value(&p).unwrap(),
        json!({
            "type": "chatgptAuthTokens",
            "accessToken": "access-token",
            "chatgptAccountId": "org-123",
            "chatgptPlanType": "business",
        }),
    );
}

#[test]
fn agent_message_delta_notification_method_name() {
    let n = p::ServerNotification::AgentMessageDelta(p::AgentMessageDeltaNotification {
        thread_id: "thr_1".to_string(),
        turn_id: "turn_1".to_string(),
        item_id: "item_1".to_string(),
        delta: "hi".to_string(),
        parent_item_id: None,
    });
    let v = serde_json::to_value(&n).unwrap();
    assert_eq!(v.get("method").unwrap(), &json!("item/agentMessage/delta"),);
}

#[test]
fn item_completed_notification_envelope() {
    let n = p::ServerNotification::ItemCompleted(p::ItemCompletedNotification {
        item: p::ThreadItem::AgentMessage {
            id: "msg_1".to_string(),
            text: "done".to_string(),
            phase: None,
            memory_citation: None,
        },
        thread_id: "t".to_string(),
        turn_id: "u".to_string(),
        parent_item_id: None,
    });
    let v = serde_json::to_value(&n).unwrap();
    assert_eq!(v.get("method").unwrap(), &json!("item/completed"));
    let item = v
        .get("params")
        .and_then(|p| p.get("item"))
        .expect("params.item present");
    assert_eq!(item.get("type").unwrap(), &json!("agentMessage"));
}

#[test]
fn jsonrpc_request_id_accepts_int_and_string() {
    let v_int: p::RequestId = serde_json::from_value(json!(42)).unwrap();
    let v_str: p::RequestId = serde_json::from_value(json!("abc")).unwrap();
    assert_eq!(v_int, p::RequestId::Integer(42));
    assert_eq!(v_str, p::RequestId::String("abc".to_string()));
}

#[test]
fn jsonrpc_inbound_routes_request_response_notification() {
    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "thread/start",
        "params": {},
    });
    let notif = json!({
        "jsonrpc": "2.0",
        "method": "initialized",
    });
    let resp = json!({
        "jsonrpc": "2.0",
        "id": "abc",
        "result": null,
    });
    assert!(matches!(
        p::InboundMessage::from_value(req).unwrap(),
        p::InboundMessage::Request(_)
    ));
    assert!(matches!(
        p::InboundMessage::from_value(notif).unwrap(),
        p::InboundMessage::Notification(_)
    ));
    assert!(matches!(
        p::InboundMessage::from_value(resp).unwrap(),
        p::InboundMessage::Response(_)
    ));
}

#[test]
fn ask_for_approval_kebab_case() {
    let v = serde_json::to_value(p::AskForApproval::OnFailure).unwrap();
    assert_eq!(v, json!("on-failure"));
    let unt = serde_json::to_value(p::AskForApproval::UnlessTrusted).unwrap();
    assert_eq!(unt, json!("untrusted"));
}

#[test]
fn sandbox_mode_kebab_case() {
    assert_eq!(
        serde_json::to_value(p::SandboxMode::WorkspaceWrite).unwrap(),
        json!("workspace-write"),
    );
}

#[test]
fn approvals_reviewer_accepts_legacy_alias() {
    let v: p::ApprovalsReviewer = serde_json::from_value(json!("guardian_subagent")).unwrap();
    assert_eq!(v, p::ApprovalsReviewer::AutoReview);
}
