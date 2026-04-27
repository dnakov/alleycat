//! V6 verification matrix: `thread/list` cwd filter behavior.
//!
//! Seeds three pi sessions across three different cwds via `PiHomeFixture`,
//! then drives `handle_thread_list` in-process (same pattern as V1 — main.rs's
//! `thread/*` dispatch lives in #32 and is covered by handlers' V8). Asserts:
//!
//! - `cwd: Some(One(a))` returns exactly one Thread with `cwd == a`.
//! - `cwd: None` returns all three.
//! - `cwd: Some(Many([a, b]))` returns exactly two (codex's
//!   `ThreadListCwdFilter::Many` is wire-encoded as a JSON string array, which
//!   the bridge handler decodes via `parse_cwd_filter`).

mod support;

use std::sync::Arc;

use alleycat_pi_bridge::codex_proto as p;
use alleycat_pi_bridge::handlers;
use alleycat_pi_bridge::index::ThreadIndex;
use alleycat_pi_bridge::pool::PiPool;
use alleycat_pi_bridge::state::{ConnectionState, ThreadDefaults};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::mpsc;

use support::{PiHomeFixture, fake_pi_path};

/// Make a minimal pi-shape session JSONL: a `session` header + a single user
/// message so `firstMessage` and `messageCount` are non-trivial. Returns the
/// list of `serde_json::Value` entries; the caller passes them to
/// `PiHomeFixture::seed_session`.
fn seed_entries(session_id: &str, cwd: &str) -> Vec<Value> {
    vec![
        json!({
            "type": "session",
            "version": 3,
            "id": session_id,
            "timestamp": "2026-04-27T10:00:00Z",
            "cwd": cwd
        }),
        json!({
            "type": "message",
            "id": format!("{session_id}-m1"),
            "parentId": null,
            "timestamp": "2026-04-27T10:00:05Z",
            "message": {"role": "user", "content": "hello"}
        }),
    ]
}

/// Build the bridge state used by these tests. The pool's `pi_bin` points at
/// fake-pi but never gets spawned because `thread/list` only consults the
/// index. `NoopThreadIndex` would defeat the point — we want the real
/// `ThreadIndex`, hydrated from disk.
async fn build_state(home: &PiHomeFixture) -> Arc<ConnectionState> {
    let codex_home = TempDir::new().unwrap();
    let index = ThreadIndex::open(codex_home.path()).await.unwrap();
    // Hydrate from the seeded sessions directory under PI_CODING_AGENT_DIR.
    index
        .hydrate_from_pi_dir(Some(&home.sessions_dir()))
        .await
        .unwrap();
    // Keep the codex_home tempdir alive past `await` boundaries — once
    // ThreadIndex captures the file path, dropping `codex_home` would race
    // the persist on shutdown. Leak the guard for the test's lifetime.
    std::mem::forget(codex_home);

    let (tx, _rx) = mpsc::unbounded_channel::<p::JsonRpcMessage>();
    let pool = Arc::new(PiPool::new(fake_pi_path()));
    Arc::new(ConnectionState::new(
        tx,
        pool,
        index,
        ThreadDefaults::default(),
    ))
}

#[tokio::test]
async fn thread_list_cwd_filter_single_string_returns_only_matching() {
    let home = PiHomeFixture::new();
    let cwd_a = "/work/proj-a";
    let cwd_b = "/work/proj-b";
    let cwd_c = "/work/proj-c";
    home.seed_session("encoded-a", "sess-a", &seed_entries("pi-a", cwd_a));
    home.seed_session("encoded-b", "sess-b", &seed_entries("pi-b", cwd_b));
    home.seed_session("encoded-c", "sess-c", &seed_entries("pi-c", cwd_c));

    let state = build_state(&home).await;

    // Filter by cwd_a as a single string → exactly one Thread, with cwd == a.
    let resp = handlers::thread::handle_thread_list(
        &state,
        p::ThreadListParams {
            cwd: Some(Value::String(cwd_a.to_string())),
            ..Default::default()
        },
    )
    .await
    .expect("thread/list should succeed");
    assert_eq!(resp.data.len(), 1, "got {:?}", resp.data);
    assert_eq!(resp.data[0].cwd, cwd_a);
}

#[tokio::test]
async fn thread_list_no_filter_returns_all() {
    let home = PiHomeFixture::new();
    home.seed_session("e-a", "sess-a", &seed_entries("pi-a", "/work/a"));
    home.seed_session("e-b", "sess-b", &seed_entries("pi-b", "/work/b"));
    home.seed_session("e-c", "sess-c", &seed_entries("pi-c", "/work/c"));

    let state = build_state(&home).await;

    let resp = handlers::thread::handle_thread_list(&state, p::ThreadListParams::default())
        .await
        .expect("thread/list");
    assert_eq!(resp.data.len(), 3);
    let mut cwds: Vec<&str> = resp.data.iter().map(|t| t.cwd.as_str()).collect();
    cwds.sort();
    assert_eq!(cwds, vec!["/work/a", "/work/b", "/work/c"]);
}

#[tokio::test]
async fn thread_list_cwd_filter_array_returns_subset() {
    let home = PiHomeFixture::new();
    let cwd_a = "/work/a";
    let cwd_b = "/work/b";
    let cwd_c = "/work/c";
    home.seed_session("e-a", "sess-a", &seed_entries("pi-a", cwd_a));
    home.seed_session("e-b", "sess-b", &seed_entries("pi-b", cwd_b));
    home.seed_session("e-c", "sess-c", &seed_entries("pi-c", cwd_c));

    let state = build_state(&home).await;

    // Array form (`ThreadListCwdFilter::Many` on the codex side, `Vec<String>`
    // through `parse_cwd_filter`).
    let resp = handlers::thread::handle_thread_list(
        &state,
        p::ThreadListParams {
            cwd: Some(json!([cwd_a, cwd_b])),
            ..Default::default()
        },
    )
    .await
    .expect("thread/list");
    assert_eq!(resp.data.len(), 2, "got {:?}", resp.data);
    let mut cwds: Vec<&str> = resp.data.iter().map(|t| t.cwd.as_str()).collect();
    cwds.sort();
    assert_eq!(cwds, vec![cwd_a, cwd_b]);
}
