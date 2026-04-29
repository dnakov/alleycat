//! The canonical conformance scenario.
//!
//! Each target runs *the same* sequence of JSON-RPC ops. We assert on shape
//! (typed deserialize + key-set diff), not LLM-generated content, so the
//! scenario is engineered to keep tool-use and reasoning to a minimum: a
//! one-shot prompt that any minimally-capable agent can answer with a single
//! short assistant message.
//!
//! The scenario records every response and every notification it sees into
//! a [`Transcript`]. It tolerates per-op failures — a `MethodNotFound` reply
//! is still a Frame, and the diff layer's `KnownDivergence` table decides
//! whether that's expected.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::cache;
use crate::transport::JsonRpcClient;
use crate::{Frame, FrameKind, TargetId, Transcript};

/// Configuration knobs each runner can tune.
#[derive(Debug, Clone)]
pub struct ScenarioConfig {
    /// Working directory passed to `thread/start.cwd` and used as the cwd for
    /// `command/exec`. Should be a fresh tempdir owned by the test.
    pub cwd: PathBuf,
    /// Deadline for "small" rpcs (initialize, list calls, thread/start).
    pub default_deadline: Duration,
    /// Deadline for `turn/start` → `turn/completed` drain.
    pub turn_deadline: Duration,
    /// After each request, wait this long for async notifications to arrive
    /// before moving to the next step. Codex emits things like
    /// `thread/status/changed` and `account/rateLimits/updated` async; pi
    /// and claude emit them synchronously. Without a drain window async
    /// notifications get attributed to the next step's drain.
    pub post_request_idle: Duration,
    /// Prompt sent on the simple-reply `turn/start`. Tuned to keep the
    /// response short and shape-stable across providers.
    pub prompt: String,
    /// Prompt sent on the tool-using `turn/start`. Engineered so each
    /// agent picks its shell tool (no model has a meaningful non-tool
    /// answer to "what's the literal stdout of this exact command").
    pub tool_prompt: String,
    /// Client-info name advertised on `initialize`.
    pub client_name: String,
    /// Client-info version advertised on `initialize`.
    pub client_version: String,
}

impl ScenarioConfig {
    pub fn for_target(target: TargetId, cwd: PathBuf) -> Self {
        Self {
            cwd,
            default_deadline: Duration::from_secs(20),
            turn_deadline: Duration::from_secs(60),
            post_request_idle: Duration::from_millis(150),
            prompt: "Reply with exactly the word OK and nothing else.".to_string(),
            tool_prompt: String::new(), // populated per-run by `run` so the
            // marker token is unique each time.
            client_name: format!("alleycat-bridge-conformance/{}", target.label()),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

/// Pick a fresh random marker token. Used in the tool-call prompt so
/// the model can't sidestep tool use by memoizing — the marker doesn't
/// exist anywhere except the file we write into the conformance cwd.
fn fresh_marker() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("alleycat-conformance-{nanos:x}")
}

/// Run the full scenario against one client. The transport may fail mid-
/// scenario; transcripts are returned partial in that case via the Err
/// variant containing what we managed to capture.
pub async fn run(
    client: &mut JsonRpcClient,
    cfg: &ScenarioConfig,
    target: TargetId,
) -> Result<Transcript> {
    let mut t = Transcript::new(target);

    // Write a marker file with a unique payload into the conformance cwd.
    // The tool-using turn asks the model to `cat` it; without the marker the
    // smarter models (gpt-5.5) just guess a plausible answer ("conformance")
    // and skip the tool call entirely, which means we never get to compare
    // the CommandExecution wire shape across bridges.
    let marker_token = fresh_marker();
    let marker_path = cfg.cwd.join("conformance-marker.txt");
    if let Err(err) = std::fs::write(&marker_path, &marker_token) {
        tracing::warn!(?err, path = %marker_path.display(), "failed to write marker file");
    }
    let tool_prompt = if cfg.tool_prompt.is_empty() {
        format!(
            "Use a shell command to read the file `conformance-marker.txt` in the current working directory and report the literal contents. The file exists; you must run the command (e.g. `cat conformance-marker.txt`). Do not guess or skip the tool — the contents are random and unguessable."
        )
    } else {
        cfg.tool_prompt.clone()
    };
    let _ = &marker_token; // returned via the file; not directly compared

    // Step 1: initialize ----------------------------------------------------
    let init = client
        .request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": cfg.client_name,
                    "version": cfg.client_version,
                },
                // Opt into experimental APIs so methods like
                // `collaborationMode/list` are available on codex (which
                // gates them behind this flag).
                "capabilities": { "experimentalApi": true },
            }),
            cfg.default_deadline,
        )
        .await
        .context("initialize")?;
    push_response(&mut t, "initialize", "initialize", &init.response);
    push_notifications(&mut t, "initialize", &init.notifications);

    // Step 2: initialized notification (no response) ------------------------
    client
        .notify("initialized", None)
        .await
        .context("initialized")?;

    // Step 3: read-only fan-out --------------------------------------------
    for (step, method, params) in READ_ONLY_OPS {
        let outcome = client
            .request(method, params(), cfg.default_deadline)
            .await
            .with_context(|| format!("{method}"))?;
        push_response(&mut t, step, method, &outcome.response);
        let trailing = client.drain_idle(cfg.post_request_idle).await;
        push_notifications(&mut t, step, &outcome.notifications);
        push_notifications(&mut t, step, &trailing);
    }

    // Step 4: thread/list (with explicit archived=false, exercises the param) -
    let listed = client
        .request(
            "thread/list",
            json!({ "archived": false }),
            cfg.default_deadline,
        )
        .await
        .context("thread/list")?;
    push_response(&mut t, "thread/list", "thread/list", &listed.response);
    let trailing = client.drain_idle(cfg.post_request_idle).await;
    push_notifications(&mut t, "thread/list", &listed.notifications);
    push_notifications(&mut t, "thread/list", &trailing);

    // Step 5: resume the cached test thread, or start a new one ------------
    //
    // Per-target thread persistence: we keep one canonical "conformance"
    // thread per target across runs (cached in `~/.cache/alleycat-bridge-
    // conformance/threads.json`) so the harness doesn't pollute the user's
    // real thread list with a fresh row every time. First run on a clean
    // machine creates the thread; every later run resumes it.
    //
    // approvalPolicy=never + sandbox=danger-full-access so the second turn's
    // shell tool call runs without bouncing a server→client permission
    // prompt the harness has no way to answer.
    let thread_start_params = json!({
        "cwd": cfg.cwd.to_string_lossy(),
        "approvalPolicy": "never",
        "sandbox": "danger-full-access",
    });

    // Label the captured frame with the method we actually call, so the
    // diff layer compares "codex thread/resume" to "bridge thread/resume"
    // (not to "bridge thread/start", which would mismatch shapes).
    let cached_id = cache::load_thread_id(target);
    let (resumed_id, attach_method, attach_response) = if let Some(id) = cached_id.clone() {
        let resume_params = json!({
            "threadId": id,
            "cwd": cfg.cwd.to_string_lossy(),
            "approvalPolicy": "never",
            "sandbox": "danger-full-access",
        });
        match client
            .request("thread/resume", resume_params, cfg.default_deadline)
            .await
        {
            Ok(out) if frame_is_error(&out.response).is_none() => (Some(id), "thread/resume", out),
            other => {
                if let Ok(out) = &other {
                    if let Some((code, msg)) = frame_is_error(&out.response) {
                        tracing::info!(
                            cached = %id,
                            code,
                            msg = %msg,
                            "cached thread resume failed; will thread/start fresh"
                        );
                    }
                }
                let _ = cache::clear_thread_id(target);
                (
                    None,
                    "thread/start",
                    client
                        .request(
                            "thread/start",
                            thread_start_params.clone(),
                            cfg.default_deadline,
                        )
                        .await
                        .context("thread/start")?,
                )
            }
        }
    } else {
        let out = client
            .request(
                "thread/start",
                thread_start_params.clone(),
                cfg.default_deadline,
            )
            .await
            .context("thread/start")?;
        (None, "thread/start", out)
    };
    push_response(
        &mut t,
        attach_method,
        attach_method,
        &attach_response.response,
    );
    let trailing = client.drain_idle(cfg.post_request_idle).await;
    push_notifications(&mut t, attach_method, &attach_response.notifications);
    push_notifications(&mut t, attach_method, &trailing);

    let thread_id = match resumed_id.or_else(|| extract_thread_id(&attach_response.response)) {
        Some(id) => id,
        None => {
            tracing::warn!(target = %target, "thread attach did not return thread.id; aborting scenario");
            return Ok(t);
        }
    };
    if cached_id.as_deref() != Some(thread_id.as_str()) {
        if let Err(err) = cache::save_thread_id(target, &thread_id) {
            tracing::warn!(?err, "failed to persist conformance thread id");
        }
    }

    // Step 6: turn/start + drain until turn/completed -----------------------
    let turn = client
        .request(
            "turn/start",
            json!({
                "threadId": thread_id,
                "input": [{ "type": "text", "text": cfg.prompt }],
                "approvalPolicy": "never",
                "sandbox": "danger-full-access",
            }),
            cfg.default_deadline,
        )
        .await
        .context("turn/start")?;
    push_response(&mut t, "turn/start", "turn/start", &turn.response);
    push_notifications(&mut t, "turn/start", &turn.notifications);

    let drain = client
        .drain_notifications_until(&["turn/completed"], cfg.turn_deadline)
        .await
        .context("draining turn/start")?;
    push_notifications(&mut t, "turn/start", &drain.notifications);

    // Step 7: thread/read ---------------------------------------------------
    let read = client
        .request(
            "thread/read",
            json!({ "threadId": thread_id, "includeTurns": true }),
            cfg.default_deadline,
        )
        .await
        .context("thread/read")?;
    push_response(&mut t, "thread/read", "thread/read", &read.response);
    let trailing = client.drain_idle(cfg.post_request_idle).await;
    push_notifications(&mut t, "thread/read", &read.notifications);
    push_notifications(&mut t, "thread/read", &trailing);

    // Step 7a: turn/start with a tool-using prompt --------------------------
    //
    // Exercises the CommandExecution / DynamicToolCall item shapes that the
    // simple-reply turn doesn't reach. Prompt is engineered so any of the
    // four agents picks their shell tool (claude→Bash, opencode→bash,
    // pi→shell, codex→shell) without a thinking detour. The bridges'
    // `approvalPolicy: "never"` (set on thread/start) lets the call run
    // without surfacing a server→client permission request.
    let tool_turn = client
        .request(
            "turn/start",
            json!({
                "threadId": thread_id,
                "input": [{
                    "type": "text",
                    "text": tool_prompt,
                }],
                "approvalPolicy": "never",
                "sandbox": "danger-full-access",
            }),
            cfg.default_deadline,
        )
        .await
        .context("turn/start (tool)")?;
    push_response(&mut t, "turn/start.tool", "turn/start", &tool_turn.response);
    push_notifications(&mut t, "turn/start.tool", &tool_turn.notifications);
    let drain = client
        .drain_notifications_until(&["turn/completed"], cfg.turn_deadline)
        .await
        .context("draining turn/start (tool)")?;
    push_notifications(&mut t, "turn/start.tool", &drain.notifications);

    // Step 7b: thread/read after the tool-use turn -------------------------
    let read_after_tool = client
        .request(
            "thread/read",
            json!({ "threadId": thread_id, "includeTurns": true }),
            cfg.default_deadline,
        )
        .await
        .context("thread/read (after tool)")?;
    push_response(
        &mut t,
        "thread/read.afterTool",
        "thread/read",
        &read_after_tool.response,
    );
    let trailing = client.drain_idle(cfg.post_request_idle).await;
    push_notifications(
        &mut t,
        "thread/read.afterTool",
        &read_after_tool.notifications,
    );
    push_notifications(&mut t, "thread/read.afterTool", &trailing);

    // Step 7c: thread/resume — re-attach to the same thread. Exercises a
    // separate codex code path (resumes the rollout into a fresh session)
    // and is shape-checked field-by-field against codex.
    let resume = client
        .request(
            "thread/resume",
            json!({ "threadId": thread_id }),
            cfg.default_deadline,
        )
        .await
        .context("thread/resume")?;
    push_response(&mut t, "thread/resume", "thread/resume", &resume.response);
    let trailing = client.drain_idle(cfg.post_request_idle).await;
    push_notifications(&mut t, "thread/resume", &resume.notifications);
    push_notifications(&mut t, "thread/resume", &trailing);

    // Step 8: thread/name/set ----------------------------------------------
    let rename = client
        .request(
            "thread/name/set",
            json!({ "threadId": thread_id, "name": "conformance" }),
            cfg.default_deadline,
        )
        .await
        .context("thread/name/set")?;
    push_response(
        &mut t,
        "thread/name/set",
        "thread/name/set",
        &rename.response,
    );
    let trailing = client.drain_idle(cfg.post_request_idle).await;
    push_notifications(&mut t, "thread/name/set", &rename.notifications);
    push_notifications(&mut t, "thread/name/set", &trailing);

    // (archive/unarchive steps intentionally omitted — those operations
    // mutate the user's persisted thread index. Each conformance run would
    // otherwise leave the test thread archived in the user's real opencode
    // / claude / pi history. Wire shape for these methods is verified
    // separately by the typed-decode unit tests in `codex-proto`.)

    // Step 9: command/exec -- a known-deterministic shell command. We
    // deliberately don't set streamStdoutStderr because pi-bridge requires
    // a client-supplied processId in that mode; the buffered shape is the
    // common-denominator path every bridge supports.
    let exec = client
        .request(
            "command/exec",
            json!({
                "command": ["sh", "-c", "printf hello"],
            }),
            cfg.default_deadline,
        )
        .await
        .context("command/exec")?;
    push_response(&mut t, "command/exec", "command/exec", &exec.response);
    let trailing = client.drain_idle(cfg.post_request_idle).await;
    push_notifications(&mut t, "command/exec", &exec.notifications);
    push_notifications(&mut t, "command/exec", &trailing);

    Ok(t)
}

type ParamsBuilder = fn() -> Value;
const READ_ONLY_OPS: &[(&str, &str, ParamsBuilder)] = &[
    ("config/read", "config/read", || json!({})),
    ("model/list", "model/list", || json!({})),
    (
        "experimentalFeature/list",
        "experimentalFeature/list",
        || json!({}),
    ),
    ("collaborationMode/list", "collaborationMode/list", || {
        json!({})
    }),
    ("mcpServerStatus/list", "mcpServerStatus/list", || json!({})),
    ("skills/list", "skills/list", || json!({})),
    ("account/read", "account/read", || json!({})),
];

fn push_response(t: &mut Transcript, step: &str, method: &str, raw: &Value) {
    t.push(Frame {
        step: step.to_string(),
        kind: FrameKind::Response,
        method: method.to_string(),
        raw: raw.clone(),
    });
}

fn push_notifications(t: &mut Transcript, step: &str, notifs: &[Value]) {
    for n in notifs {
        let method = n
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("(missing)")
            .to_string();
        t.push(Frame {
            step: step.to_string(),
            kind: FrameKind::Notification,
            method,
            raw: n.clone(),
        });
    }
}

fn extract_thread_id(response: &Value) -> Option<String> {
    response
        .get("result")?
        .get("thread")?
        .get("id")?
        .as_str()
        .map(str::to_string)
}

/// `(code, message)` if the response is a JSON-RPC error envelope.
fn frame_is_error(response: &Value) -> Option<(i64, String)> {
    let err = response.get("error")?;
    let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
    let message = err
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("(no message)")
        .to_string();
    Some((code, message))
}
