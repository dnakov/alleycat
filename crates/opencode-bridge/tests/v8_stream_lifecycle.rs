//! T2 acceptance: the bridge dispatches `turn/start` via `prompt_async` and
//! lets SSE events drive the lifecycle. This test proves the async ack — the
//! `turn/start` response returns before opencode has emitted any items, and
//! `turn/completed` is gated on the SSE `session.idle` event rather than on
//! the upstream HTTP response.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use alleycat_bridge_core::framing::{read_json_line, write_json_line};
use alleycat_bridge_core::server::serve_stream;
use alleycat_bridge_core::{JsonRpcRequest, JsonRpcVersion, RequestId};
use alleycat_opencode_bridge::OpencodeBridge;
use alleycat_opencode_bridge::opencode_proc::OpencodeRuntime;
use serde_json::{Value, json};
use tokio::io::BufReader;

/// Channel through which the test injects SSE frames into the held-open
/// `GET /event` connection that the bridge's `SseConsumer` opened on startup.
type SseInjector = Arc<Mutex<Option<TcpStream>>>;

#[tokio::test]
async fn turn_start_acks_immediately_then_session_idle_completes_turn() {
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let sse_injector: SseInjector = Arc::new(Mutex::new(None));
    let base_url = start_fake_opencode(Arc::clone(&seen), Arc::clone(&sse_injector));
    let state_dir = tempfile::TempDir::new().unwrap();
    unsafe {
        std::env::set_var("ALLEYCAT_BRIDGE_STATE_DIR", state_dir.path());
    }
    let bridge = Arc::new(
        OpencodeBridge::new(OpencodeRuntime::external(
            base_url,
            "test-token".to_string(),
        ))
        .await
        .unwrap(),
    );

    let (client, server) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(serve_stream(bridge, server));
    let (read, mut write) = tokio::io::split(client);
    let mut read = BufReader::new(read);

    // initialize → set up bridge state.
    send(
        &mut write,
        1,
        "initialize",
        json!({"clientInfo":{"name":"v8","version":"0"}}),
    )
    .await;
    let _ = read_until_response(&mut read, 1).await;

    // The codex client signals readiness via `initialized`; this is what
    // triggers `OpencodeBridge::spawn_event_pump` so SSE events from now on
    // route into this connection.
    send_notification(&mut write, "initialized", json!({})).await;

    // Wait until the SSE consumer has opened its `/event` connection — it's
    // started during `OpencodeBridge::new`, but the fake server stashes the
    // stream so we can inject. Loop briefly until populated.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if sse_injector.lock().unwrap().is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("SSE /event never opened");

    // thread/start
    send(
        &mut write,
        2,
        "thread/start",
        json!({"cwd":"/tmp/opencode-v8","model":"openai/gpt-test"}),
    )
    .await;
    let started = read_until_response(&mut read, 2).await;
    let thread_id = started["result"]["thread"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // turn/start should return immediately with status:"inProgress".
    send(
        &mut write,
        3,
        "turn/start",
        json!({"threadId":thread_id,"input":[{"type":"text","text":"hi"}]}),
    )
    .await;
    let turn_started = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let value: Value = read_json_line(&mut read).await.unwrap().unwrap();
            // Drain notifications until we land on the response. The
            // notification we expect first is `turn/started` (emitted before
            // the prompt_async POST).
            if value.get("id").and_then(Value::as_i64) == Some(3) {
                return value;
            }
        }
    })
    .await
    .expect("turn/start response timed out");
    assert_eq!(turn_started["result"]["turn"]["status"], "inProgress");
    assert_eq!(
        turn_started["result"]["turn"]["items"]
            .as_array()
            .map(|a| a.len()),
        Some(0),
        "turn/start should ack with empty items; SSE drives the rest"
    );

    // Confirm prompt_async was the upstream call (not the legacy /message).
    let paths = seen.lock().unwrap().clone();
    assert!(
        paths
            .iter()
            .any(|p| p.starts_with("POST /session/ses_1/prompt_async")),
        "expected prompt_async in upstream calls: {paths:?}"
    );
    assert!(
        !paths
            .iter()
            .any(|p| p.starts_with("POST /session/ses_1/message")),
        "synchronous /message should no longer be used: {paths:?}"
    );

    // Inject `session.idle` via SSE; the bridge should emit `turn/completed`
    // on the codex side keyed off the active turn cached in `BridgeState`.
    inject_sse(
        &sse_injector,
        json!({"type":"session.idle","properties":{"sessionID":"ses_1"}}),
    );

    let turn_completed =
        read_until_notification(&mut read, "turn/completed", Duration::from_secs(3)).await;
    assert_eq!(turn_completed["params"]["threadId"], thread_id);
    assert_eq!(turn_completed["params"]["turn"]["status"], "completed");

    drop(write);
    server_task.abort();
    unsafe {
        std::env::remove_var("ALLEYCAT_BRIDGE_STATE_DIR");
    }
}

fn inject_sse(injector: &SseInjector, event: Value) {
    let payload = format!("data: {}\n\n", serde_json::to_string(&event).unwrap());
    let mut guard = injector.lock().unwrap();
    if let Some(stream) = guard.as_mut() {
        let _ = stream.write_all(payload.as_bytes());
        let _ = stream.flush();
    }
}

async fn send<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    id: i64,
    method: &str,
    params: Value,
) {
    write_json_line(
        writer,
        &JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id: RequestId::Integer(id),
            method: method.to_string(),
            params: Some(params),
        },
    )
    .await
    .unwrap();
}

async fn send_notification<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    method: &str,
    params: Value,
) {
    let frame = json!({"jsonrpc":"2.0","method":method,"params":params});
    let line = serde_json::to_vec(&frame).unwrap();
    use tokio::io::AsyncWriteExt;
    writer.write_all(&line).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();
}

async fn read_until_response<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R, id: i64) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let value: Value = read_json_line(reader).await.unwrap().unwrap();
            if value.get("id").and_then(Value::as_i64) == Some(id) {
                return value;
            }
        }
    })
    .await
    .unwrap()
}

async fn read_until_notification<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    method: &str,
    timeout: Duration,
) -> Value {
    tokio::time::timeout(timeout, async {
        loop {
            let value: Value = read_json_line(reader).await.unwrap().unwrap();
            if value.get("method").and_then(Value::as_str) == Some(method) {
                return value;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("notification {method} not seen within {timeout:?}"))
}

fn start_fake_opencode(seen: Arc<Mutex<Vec<String>>>, injector: SseInjector) -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let seen = Arc::clone(&seen);
            let injector = Arc::clone(&injector);
            thread::spawn(move || handle_conn(stream, seen, injector));
        }
    });
    format!("http://{addr}")
}

fn handle_conn(mut stream: TcpStream, seen: Arc<Mutex<Vec<String>>>, injector: SseInjector) {
    let mut bytes = Vec::new();
    let mut buf = [0u8; 1024];
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let headers_end = loop {
        match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                bytes.extend_from_slice(&buf[..n]);
                if let Some(pos) = find_subsequence(&bytes, b"\r\n\r\n") {
                    break pos + 4;
                }
            }
            Err(_) => return,
        }
    };
    let header_text = String::from_utf8_lossy(&bytes[..headers_end]);
    let content_length = header_text
        .lines()
        .find_map(|line| {
            line.strip_prefix("content-length:")
                .or_else(|| line.strip_prefix("Content-Length:"))
        })
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while bytes.len() < headers_end + content_length {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => bytes.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let req = String::from_utf8_lossy(&bytes);
    let request_line = req.lines().next().unwrap_or("").to_string();
    seen.lock().unwrap().push(request_line.clone());
    let _ = stream.set_read_timeout(None);

    if request_line.starts_with("GET /event") {
        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
        if stream.write_all(head.as_bytes()).is_err() {
            return;
        }
        let _ = stream.flush();
        // Hand the stream to the test for SSE injection.
        *injector.lock().unwrap() = Some(stream.try_clone().unwrap());
        // Park: keep the connection alive until torn down externally.
        thread::sleep(Duration::from_secs(60));
        return;
    }

    if request_line.starts_with("POST /session/ses_1/prompt_async") {
        let response = "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
        let _ = stream.shutdown(std::net::Shutdown::Both);
        return;
    }

    let body = if request_line.starts_with("POST /session?") {
        json!({
            "id":"ses_1",
            "directory":"/tmp/opencode-v8",
            "title":"V8",
            "time":{"created":1000,"updated":1000}
        })
    } else if request_line.starts_with("GET /provider?") {
        json!({"all":[],"default":[],"connected":[]})
    } else {
        json!({})
    };
    let body = serde_json::to_string(&body).unwrap();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
