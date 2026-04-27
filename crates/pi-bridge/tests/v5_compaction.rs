//! Verification matrix V5 — compaction flow.
//!
//! From the bridge plan (compaction row in "Pi events → codex notifications"):
//! pi `compaction_start` translates to `item/started { ContextCompaction }`,
//! pi `compaction_end { result }` to `item/completed { ContextCompaction }`
//! plus a `thread/compacted { thread_id, turn_id }`. This test drives that
//! sequence end-to-end through `EventTranslatorState`, the only piece that
//! sits between the pool's broadcast and the codex JSON-RPC writer.
//!
//! Why not go through `handlers::thread::handle_thread_compact_start`?
//! Because the dispatcher around it is in flight (handlers' #14). The
//! contract this verification file exists to lock down lives in the
//! translator + pool layer, both of which are stable. When `thread/*`
//! handlers land, an end-to-end variant that drives the same scenario via
//! codex JSON-RPC can sit alongside this one.

mod support;

use std::time::Duration;

use alleycat_pi_bridge::codex_proto::{ServerNotification, ThreadItem};
use alleycat_pi_bridge::pool::pi_protocol::{CompactCmd, PiEvent, RpcCommand};
use alleycat_pi_bridge::pool::process::PiProcessHandle;
use alleycat_pi_bridge::translate::events::EventTranslatorState;
use tempfile::TempDir;
use tokio::time::timeout;

use support::fake_pi_path;

const THREAD_ID: &str = "th_test";
const TURN_ID: &str = "tu_test";

#[tokio::test]
async fn compaction_flow_emits_item_lifecycle_and_thread_compacted() {
    let cwd = TempDir::new().unwrap();
    let handle = PiProcessHandle::spawn(cwd.path(), fake_pi_path())
        .await
        .expect("spawn fake-pi");

    let mut events = handle.subscribe_events();
    let mut translator = EventTranslatorState::new(THREAD_ID, TURN_ID);
    let mut notifications: Vec<ServerNotification> = Vec::new();

    // Subscribe before sending so we can't miss the events.
    let drain = tokio::spawn(async move {
        let mut out: Vec<PiEvent> = Vec::new();
        // We expect exactly two events: compaction_start, compaction_end.
        // Bound the wait so a regression in fake-pi shows up as a timeout.
        for _ in 0..2 {
            match timeout(Duration::from_secs(2), events.recv()).await {
                Ok(Ok(evt)) => out.push(evt),
                _ => break,
            }
        }
        out
    });

    let resp = handle
        .send_request(RpcCommand::Compact(CompactCmd {
            id: None,
            custom_instructions: None,
        }))
        .await
        .expect("compact");
    assert!(resp.success, "compact succeeded");

    let pi_events = drain.await.expect("drain task");
    assert_eq!(
        pi_events.len(),
        2,
        "expected exactly compaction_start + compaction_end"
    );

    for event in pi_events {
        notifications.extend(translator.translate(event));
    }

    // (a) `item/started { item: ContextCompaction{...} }`
    let (started_idx, started_id) = notifications
        .iter()
        .enumerate()
        .find_map(|(i, n)| match n {
            ServerNotification::ItemStarted(s) => match &s.item {
                ThreadItem::ContextCompaction { id } => Some((i, id.clone())),
                _ => None,
            },
            _ => None,
        })
        .expect("expected ItemStarted with ContextCompaction");
    assert_eq!(notifications[started_idx].method_name(), "item/started");
    assert!(!started_id.is_empty(), "ContextCompaction id is non-empty");

    // (b) `item/completed` for the same ContextCompaction id.
    let completed_id = notifications
        .iter()
        .find_map(|n| match n {
            ServerNotification::ItemCompleted(c) => match &c.item {
                ThreadItem::ContextCompaction { id } => Some(id.clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("expected ItemCompleted with ContextCompaction");
    assert_eq!(
        completed_id, started_id,
        "ItemCompleted should reuse the ItemStarted id"
    );

    // (c) `thread/compacted { thread_id, turn_id }`.
    let compacted = notifications
        .iter()
        .find_map(|n| match n {
            ServerNotification::ContextCompacted(c) => Some(c),
            _ => None,
        })
        .expect("expected thread/compacted notification");
    assert_eq!(compacted.thread_id, THREAD_ID);
    assert_eq!(compacted.turn_id, TURN_ID);

    // Ordering: started must precede completed must precede compacted.
    let positions: Vec<usize> = notifications
        .iter()
        .enumerate()
        .filter_map(|(i, n)| match n {
            ServerNotification::ItemStarted(_)
            | ServerNotification::ItemCompleted(_)
            | ServerNotification::ContextCompacted(_) => Some(i),
            _ => None,
        })
        .collect();
    assert_eq!(
        positions.len(),
        3,
        "expected three lifecycle notifications, got {}",
        positions.len()
    );
    assert!(positions.windows(2).all(|w| w[0] < w[1]));

    // Healthy compaction (aborted=false, error_message=None) should NOT emit
    // an `error` notification. The translator only fires that on aborted /
    // errored compactions.
    assert!(
        !notifications
            .iter()
            .any(|n| matches!(n, ServerNotification::Error(_))),
        "successful compaction must not produce an Error notification"
    );

    handle.shutdown().await;
}

/// Tiny helper so the assertion above reads cleanly. Maps the variant to the
/// `method` string codex would put on the JSON-RPC frame. We don't need the
/// full string for any other assertion, just for the failure message.
trait NotificationMethodName {
    fn method_name(&self) -> &'static str;
}

impl NotificationMethodName for ServerNotification {
    fn method_name(&self) -> &'static str {
        match self {
            ServerNotification::ItemStarted(_) => "item/started",
            ServerNotification::ItemCompleted(_) => "item/completed",
            ServerNotification::ContextCompacted(_) => "thread/compacted",
            ServerNotification::AgentMessageDelta(_) => "item/agentMessage/delta",
            ServerNotification::ReasoningTextDelta(_) => "item/reasoning/textDelta",
            ServerNotification::CommandExecutionOutputDelta(_) => {
                "item/commandExecution/outputDelta"
            }
            ServerNotification::McpToolCallProgress(_) => "item/mcpToolCall/progress",
            ServerNotification::Error(_) => "error",
            _ => "<other>",
        }
    }
}
