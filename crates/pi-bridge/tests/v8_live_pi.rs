//! Verification matrix V8 — live smoke against a real `pi-coding-agent`.
//!
//! **Manual / opt-in only** — every test in this file is `#[ignore]`. CI
//! does not run pi-coding-agent, has no API keys, and shouldn't be
//! pulling in pi-mono. Run locally with:
//!
//! ```sh
//! # 1. Make sure pi-coding-agent is on PATH (or override).
//! command -v pi-coding-agent
//!
//! # 2. Provide at least one model-provider key. Pi reads its catalog
//! #    from whatever providers it can reach — having any one key set is
//! #    enough.
//! export OPENAI_API_KEY=sk-...     # or ANTHROPIC_API_KEY, or GROQ_API_KEY, etc.
//!
//! # 3. (Optional) point at a specific pi binary if it's not on PATH.
//! export PI_BRIDGE_PI_BIN=/path/to/pi-coding-agent
//!
//! # 4. Run the gated test.
//! cargo test -p alleycat-pi-bridge --test v8_live_pi -- --ignored --nocapture
//! ```
//!
//! The test self-skips (passes silently) when prerequisites are missing,
//! so the same cargo invocation can run on a developer laptop without an
//! API key configured — it'll just exit early.
//!
//! ## What it asserts
//!
//! End-to-end through the deployed bridge binary:
//!
//! 1. Spawn `target/{profile}/alleycat-pi-bridge` as a child process,
//!    talk to it via stdio JSON-RPC (the same surface a codex client
//!    sees on the wire).
//! 2. Send `initialize` → expect `userAgent: alleycat-pi-bridge/<v>`.
//! 3. Send `thread/start { cwd: $PWD }` → expect a real
//!    `ThreadStartResponse` with a fresh `thread.id`.
//! 4. Send `turn/start { input:[{type:"text",text:"What's 2+2?"}] }`
//!    → expect at least one `item/agentMessage/delta` and a
//!    `turn/completed { status: "completed" }` within 30s.
//!
//! Why drive the bin instead of the handler functions directly: this is
//! the only test in the suite that exercises the JSON-RPC framing,
//! main.rs dispatcher, real pi process, real LLM, and the event-pump
//! glue all together. If anything in that pipeline regresses without
//! tripping the unit suite, V8 catches it.

use std::env;
use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;

const TURN_DEADLINE: Duration = Duration::from_secs(30);

#[tokio::test]
#[ignore = "live pi smoke — requires pi-coding-agent + API key; opt-in via --ignored"]
async fn live_pi_completes_a_simple_arithmetic_turn() {
    let Some(prereqs) = check_prereqs() else {
        eprintln!("v8_live_pi: skipped — prerequisites missing");
        return;
    };

    let bridge_bin = PathBuf::from(env!("CARGO_BIN_EXE_alleycat-pi-bridge"));
    eprintln!(
        "v8_live_pi: bridge={} pi_bin={} provider_keys={:?}",
        bridge_bin.display(),
        prereqs.pi_bin.display(),
        prereqs.provider_keys,
    );

    let mut child = Command::new(&bridge_bin)
        .env("PI_BRIDGE_PI_BIN", &prereqs.pi_bin)
        // Forward whatever provider keys the developer has set so pi can
        // actually reach a model. We don't filter — anything pi-mono
        // recognizes goes through verbatim.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn alleycat-pi-bridge");

    let mut stdin = child.stdin.take().expect("child stdin");
    let stdout = child.stdout.take().expect("child stdout");
    let stderr = child.stderr.take().expect("child stderr");

    // Tee stderr to test stderr so a failed run shows the bridge's logs
    // alongside the assertion message.
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            eprintln!("[bridge] {line}");
        }
    });

    let mut reader = BufReader::new(stdout).lines();

    // ----- 1. initialize -----
    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "clientInfo": { "name": "v8_live_pi", "version": "0.0.0" } }
        }),
    )
    .await;
    let init = expect_response(&mut reader, 1, Duration::from_secs(5)).await;
    let user_agent = init["result"]["userAgent"].as_str().unwrap_or("");
    assert!(
        user_agent.starts_with("alleycat-pi-bridge/"),
        "userAgent should identify the bridge, got: {user_agent:?}"
    );

    // ----- 2. thread/start -----
    let cwd = std::env::current_dir().expect("cwd");
    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "thread/start",
            "params": { "cwd": cwd.to_string_lossy() }
        }),
    )
    .await;
    let start = expect_response(&mut reader, 2, Duration::from_secs(15)).await;
    assert!(
        start["error"].is_null(),
        "thread/start should not error: {start}"
    );
    let thread_id = start["result"]["thread"]["id"]
        .as_str()
        .expect("thread.id present")
        .to_string();
    eprintln!("v8_live_pi: thread/start ok, thread_id={thread_id}");

    // ----- 3. turn/start -----
    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "turn/start",
            "params": {
                "threadId": thread_id,
                "input": [{ "type": "text", "text": "What's 2+2? Reply with just the number." }]
            }
        }),
    )
    .await;

    // The turn/start response itself comes back fast (right after pi's
    // preflight ack). The `item/agentMessage/delta` notifications and
    // `turn/completed` arrive as the LLM responds.
    let mut saw_turn_response = false;
    let mut saw_message_delta = false;
    let mut turn_completed_status: Option<String> = None;
    let started = Instant::now();

    loop {
        if started.elapsed() > TURN_DEADLINE {
            panic!(
                "v8_live_pi: deadline exceeded; saw_turn_response={saw_turn_response} \
                 saw_message_delta={saw_message_delta} turn_completed={turn_completed_status:?}"
            );
        }
        let remaining = TURN_DEADLINE.saturating_sub(started.elapsed());
        let line = match timeout(remaining, reader.next_line()).await {
            Ok(Ok(Some(l))) => l,
            Ok(Ok(None)) => panic!("v8_live_pi: bridge stdout closed mid-turn"),
            Ok(Err(err)) => panic!("v8_live_pi: stdout read error: {err}"),
            Err(_) => continue, // re-check deadline above
        };
        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Distinguish responses (have `id`+`result`/`error`) from
        // notifications (have `method`+`params`, no `id`).
        if value.get("id") == Some(&json!(3)) {
            saw_turn_response = true;
            assert!(
                value["error"].is_null(),
                "turn/start should not error: {value}"
            );
            continue;
        }
        let method = value["method"].as_str().unwrap_or("");
        match method {
            "item/agentMessage/delta" => saw_message_delta = true,
            "turn/completed" => {
                let status = value["params"]["turn"]["status"]
                    .as_str()
                    .unwrap_or("(missing)")
                    .to_string();
                turn_completed_status = Some(status);
                break;
            }
            "error" => {
                panic!("v8_live_pi: bridge emitted error notification mid-turn: {value}");
            }
            _ => {}
        }
    }

    assert!(saw_turn_response, "expected turn/start response with id=3");
    assert!(
        saw_message_delta,
        "expected at least one item/agentMessage/delta during the turn"
    );
    assert_eq!(
        turn_completed_status.as_deref(),
        Some("completed"),
        "turn/completed.status should be \"completed\""
    );

    // Clean shutdown — closing stdin makes the bridge fall out of its
    // dispatch loop, which makes pi exit, which lets the child reap.
    drop(stdin);
    let _ = timeout(Duration::from_secs(5), child.wait()).await;
}

// ============================================================================
// helpers
// ============================================================================

struct Prereqs {
    pi_bin: PathBuf,
    provider_keys: Vec<&'static str>,
}

fn check_prereqs() -> Option<Prereqs> {
    let pi_bin = match env::var_os("PI_BRIDGE_PI_BIN") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => match which("pi-coding-agent") {
            Some(p) => p,
            None => {
                eprintln!("v8_live_pi: pi-coding-agent not on PATH and PI_BRIDGE_PI_BIN unset");
                return None;
            }
        },
    };
    if !pi_bin.exists() {
        eprintln!("v8_live_pi: pi binary {} does not exist", pi_bin.display());
        return None;
    }

    // Pi recognizes any of these envs to reach an LLM. We just need *one*.
    const KEYS: &[&str] = &[
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "GROQ_API_KEY",
        "AZURE_OPENAI_API_KEY",
        "GOOGLE_API_KEY",
    ];
    let provider_keys: Vec<&'static str> = KEYS
        .iter()
        .copied()
        .filter(|k| env::var(k).ok().map(|v| !v.is_empty()).unwrap_or(false))
        .collect();
    if provider_keys.is_empty() {
        eprintln!("v8_live_pi: no model-provider key found; set one of: {KEYS:?}",);
        return None;
    }

    Some(Prereqs {
        pi_bin,
        provider_keys,
    })
}

/// Minimal `which` so we don't pull in the `which` crate just for one
/// PATH lookup. Returns the first match in `PATH` whose name equals
/// `bin` and which is executable on the current platform.
fn which(bin: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

async fn send(stdin: &mut tokio::process::ChildStdin, value: &Value) {
    let mut line = serde_json::to_vec(value).expect("serialize JSON-RPC frame");
    line.push(b'\n');
    stdin.write_all(&line).await.expect("write to bridge stdin");
    stdin.flush().await.expect("flush bridge stdin");
}

async fn expect_response(
    reader: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    id: i64,
    deadline: Duration,
) -> Value {
    let started = Instant::now();
    loop {
        let remaining = deadline
            .checked_sub(started.elapsed())
            .unwrap_or(Duration::ZERO);
        if remaining.is_zero() {
            panic!("v8_live_pi: timed out waiting for response id={id}");
        }
        let line = match timeout(remaining, reader.next_line()).await {
            Ok(Ok(Some(l))) => l,
            Ok(Ok(None)) => panic!("v8_live_pi: bridge stdout closed before response id={id}"),
            Ok(Err(err)) => panic!("v8_live_pi: read error: {err}"),
            Err(_) => continue,
        };
        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if value.get("id") == Some(&json!(id)) {
            return value;
        }
        // Not our response — likely a startup notification. Skip and
        // keep reading.
    }
}

// Suppress unused-import warning when not running this binary on a
// platform where one of the `use` items isn't reached.
#[allow(dead_code)]
fn _silence_unused_use_warnings() {
    let _ = std::io::stdout().flush();
}
