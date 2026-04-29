//! Wire-compat regression: the on-disk `threads.json` shape must stay
//! byte-compatible with the pre-A2 layout. Pi's index used to define its own
//! `IndexEntry` with `pi_session_path` / `pi_session_id` directly; A2 lifts
//! the storage into `bridge_core::ThreadIndex<PiSessionRef>` with those two
//! fields living on a flattened `metadata: PiSessionRef`. The flatten must
//! produce JSON identical to the old shape — a single key rename or a
//! missing camelCase translation would silently invalidate every existing
//! `threads.json` on disk.

use std::path::PathBuf;

use alleycat_pi_bridge::codex_proto::ThreadSourceKind;
use alleycat_pi_bridge::index::{IndexEntry, PiSessionRef, ThreadIndex};
use serde_json::json;

fn sample_entry(thread_id: &str) -> IndexEntry {
    IndexEntry {
        thread_id: thread_id.to_string(),
        cwd: "/work/project".to_string(),
        created_at: 1_700_000_000_000,
        updated_at: 1_700_000_900_000,
        archived: true,
        name: Some("My thread".to_string()),
        preview: "first message preview".to_string(),
        forked_from_id: Some("parent-thread".to_string()),
        model_provider: "pi".to_string(),
        source: ThreadSourceKind::AppServer,
        metadata: PiSessionRef {
            pi_session_path: PathBuf::from("/Users/me/.pi/agent/sessions/encoded/abc.jsonl"),
            pi_session_id: "pi-session-abc".to_string(),
        },
    }
}

#[test]
fn serialized_row_matches_pre_a2_shape() {
    // Expected on-disk shape: pi-specific fields flat at the top level (not
    // nested under `metadata`), all keys camelCase.
    let entry = sample_entry("thread-123");
    let value = serde_json::to_value(&entry).unwrap();
    assert_eq!(
        value,
        json!({
            "threadId": "thread-123",
            "cwd": "/work/project",
            "createdAt": 1_700_000_000_000_i64,
            "updatedAt": 1_700_000_900_000_i64,
            "archived": true,
            "name": "My thread",
            "preview": "first message preview",
            "forkedFromId": "parent-thread",
            "modelProvider": "pi",
            "source": "appServer",
            "piSessionPath": "/Users/me/.pi/agent/sessions/encoded/abc.jsonl",
            "piSessionId": "pi-session-abc",
        }),
    );
}

#[test]
fn pre_a2_rows_deserialize_unchanged() {
    // Take a row written by the pre-A2 pi-bridge and confirm the new
    // `IndexEntry<PiSessionRef>` deserializes it into the same logical
    // values the old struct held.
    let raw = json!({
        "threadId": "thread-123",
        "cwd": "/work/project",
        "createdAt": 1_700_000_000_000_i64,
        "updatedAt": 1_700_000_900_000_i64,
        "archived": false,
        "preview": "first message",
        "modelProvider": "pi",
        "source": "appServer",
        "piSessionPath": "/Users/me/.pi/agent/sessions/encoded/abc.jsonl",
        "piSessionId": "pi-session-abc",
    });
    let parsed: IndexEntry = serde_json::from_value(raw).unwrap();
    assert_eq!(parsed.thread_id, "thread-123");
    assert_eq!(parsed.cwd, "/work/project");
    assert_eq!(parsed.created_at, 1_700_000_000_000);
    assert!(!parsed.archived);
    assert_eq!(parsed.name, None);
    assert_eq!(parsed.forked_from_id, None);
    assert_eq!(parsed.metadata.pi_session_id, "pi-session-abc");
    assert_eq!(
        parsed.metadata.pi_session_path,
        PathBuf::from("/Users/me/.pi/agent/sessions/encoded/abc.jsonl")
    );
}

#[tokio::test]
async fn full_round_trip_through_threads_json() {
    // Write a row, persist to disk, re-open. Both the on-disk JSON and the
    // re-parsed entry must match the original.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("threads.json");
    let index = ThreadIndex::open_at(path.clone()).await.unwrap();

    let entry = sample_entry("rt-thread");
    index.insert(entry.clone()).await.unwrap();

    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.contains("\"piSessionPath\""));
    assert!(raw.contains("\"piSessionId\""));
    assert!(!raw.contains("\"metadata\""));

    drop(index);
    let reopened = ThreadIndex::open_at(path).await.unwrap();
    let row = reopened.lookup("rt-thread").await.unwrap();
    assert_eq!(row, entry);
}
