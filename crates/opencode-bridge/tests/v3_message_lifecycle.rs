//! T3 acceptance: SSE `message.updated` and `message.part.updated` events drive
//! the codex `item/started` / `item/completed` lifecycle. Two scenarios:
//!
//! 1. Assistant message: `message.updated` (first sighting) → `item/started`
//!    AgentMessage; `message.part.updated{type:text}` caches the part kind;
//!    `message.part.delta{field:text}` accumulates and emits
//!    `item/agentMessage/delta`; `message.updated` with `info.time.completed`
//!    set → `item/completed` with the full accumulated text.
//!
//! 2. Bash tool: `message.part.updated{type:tool, tool:bash, state:running}`
//!    → `item/started{type:commandExecution}`. Same part with
//!    `state:completed` → `item/completed` with `aggregatedOutput` populated.

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

type SseInjector = Arc<Mutex<Option<TcpStream>>>;

#[tokio::test]
async fn assistant_message_streams_started_delta_completed() {
    let (mut read, mut write, sse_injector, _state_dir, server_task) =
        bring_up_bridge("v3-text").await;

    // thread/start → bind ses_1.
    let thread_id = start_thread(&mut read, &mut write, "/tmp/opencode-v3").await;

    // turn/start to set the active turn (so route_event has a turn id to
    // attribute notifications to).
    send(
        &mut write,
        3,
        "turn/start",
        json!({"threadId":thread_id,"input":[{"type":"text","text":"hi"}]}),
    )
    .await;
    let turn_response = read_until_response(&mut read, 3).await;
    let turn_id = turn_response["result"]["turn"]["id"]
        .as_str()
        .expect("turn id")
        .to_string();

    // 1. message.updated (first sighting) — opencode emits this before any
    //    parts are present. Bridge emits `item/started` AgentMessage.
    inject_sse(
        &sse_injector,
        json!({
            "type":"message.updated",
            "properties":{
                "sessionID":"ses_1",
                "info":{"id":"msg_1","role":"assistant","time":{"created":1000}}
            }
        }),
    );
    let started = read_until_notification(&mut read, "item/started", Duration::from_secs(3)).await;
    assert_eq!(started["params"]["threadId"], thread_id);
    assert_eq!(started["params"]["turnId"], turn_id);
    assert_eq!(started["params"]["item"]["type"], "agentMessage");
    assert_eq!(started["params"]["item"]["id"], "msg_1");
    assert_eq!(started["params"]["item"]["text"], "");

    // 2. message.part.updated for the text part — caches the kind so deltas
    //    can be accumulated. (No notification expected; assistant text parts
    //    have no per-part item lifecycle.)
    inject_sse(
        &sse_injector,
        json!({
            "type":"message.part.updated",
            "properties":{
                "sessionID":"ses_1",
                "part":{"id":"part_1","messageID":"msg_1","sessionID":"ses_1","type":"text","text":""},
                "time":1001
            }
        }),
    );

    // 3. Two text deltas: each becomes `item/agentMessage/delta` AND is
    //    accumulated into the per-message buffer.
    inject_sse(
        &sse_injector,
        json!({
            "type":"message.part.delta",
            "properties":{
                "sessionID":"ses_1",
                "messageID":"msg_1",
                "partID":"part_1",
                "field":"text",
                "delta":"hello "
            }
        }),
    );
    let delta1 =
        read_until_notification(&mut read, "item/agentMessage/delta", Duration::from_secs(3)).await;
    assert_eq!(delta1["params"]["delta"], "hello ");

    inject_sse(
        &sse_injector,
        json!({
            "type":"message.part.delta",
            "properties":{
                "sessionID":"ses_1",
                "messageID":"msg_1",
                "partID":"part_1",
                "field":"text",
                "delta":"world"
            }
        }),
    );
    let delta2 =
        read_until_notification(&mut read, "item/agentMessage/delta", Duration::from_secs(3)).await;
    assert_eq!(delta2["params"]["delta"], "world");

    // 4. message.updated with info.time.completed → item/completed with the
    //    full accumulated text.
    inject_sse(
        &sse_injector,
        json!({
            "type":"message.updated",
            "properties":{
                "sessionID":"ses_1",
                "info":{"id":"msg_1","role":"assistant","time":{"created":1000,"completed":2000}}
            }
        }),
    );
    let completed =
        read_until_notification(&mut read, "item/completed", Duration::from_secs(3)).await;
    assert_eq!(completed["params"]["item"]["type"], "agentMessage");
    assert_eq!(completed["params"]["item"]["id"], "msg_1");
    assert_eq!(
        completed["params"]["item"]["text"], "hello world",
        "item/completed text should be the accumulated assistant text"
    );

    // 5. session.idle closes the turn.
    inject_sse(
        &sse_injector,
        json!({"type":"session.idle","properties":{"sessionID":"ses_1"}}),
    );
    let turn_completed =
        read_until_notification(&mut read, "turn/completed", Duration::from_secs(3)).await;
    assert_eq!(turn_completed["params"]["threadId"], thread_id);

    drop(write);
    server_task.abort();
}

#[tokio::test]
async fn bash_tool_part_emits_command_execution_started_and_completed() {
    let (mut read, mut write, sse_injector, _state_dir, server_task) =
        bring_up_bridge("v3-tool").await;

    let thread_id = start_thread(&mut read, &mut write, "/tmp/opencode-v3-tool").await;

    send(
        &mut write,
        3,
        "turn/start",
        json!({"threadId":thread_id,"input":[{"type":"text","text":"run ls"}]}),
    )
    .await;
    let _ = read_until_response(&mut read, 3).await;

    // First sighting of the bash tool part — running state. Bridge should
    // emit `item/started` with type:"commandExecution".
    inject_sse(
        &sse_injector,
        json!({
            "type":"message.part.updated",
            "properties":{
                "sessionID":"ses_1",
                "part":{
                    "id":"part_t1",
                    "callID":"call_t1",
                    "messageID":"msg_2",
                    "sessionID":"ses_1",
                    "type":"tool",
                    "tool":"bash",
                    "state":{
                        "status":"running",
                        "input":{"command":"ls -la","cwd":"/tmp/opencode-v3-tool"}
                    }
                },
                "time":1100
            }
        }),
    );
    let started = read_until_notification(&mut read, "item/started", Duration::from_secs(3)).await;
    assert_eq!(started["params"]["item"]["type"], "commandExecution");
    assert_eq!(started["params"]["item"]["id"], "call_t1");
    assert_eq!(started["params"]["item"]["command"], "ls -la");
    assert_eq!(started["params"]["item"]["cwd"], "/tmp/opencode-v3-tool");
    assert_eq!(started["params"]["item"]["status"], "running");

    // Same part now in `completed` state with `output` populated. Bridge
    // should emit `item/completed` with `aggregatedOutput`.
    inject_sse(
        &sse_injector,
        json!({
            "type":"message.part.updated",
            "properties":{
                "sessionID":"ses_1",
                "part":{
                    "id":"part_t1",
                    "callID":"call_t1",
                    "messageID":"msg_2",
                    "sessionID":"ses_1",
                    "type":"tool",
                    "tool":"bash",
                    "state":{
                        "status":"completed",
                        "input":{"command":"ls -la","cwd":"/tmp/opencode-v3-tool"},
                        "output":"a\nb\nc\n"
                    }
                },
                "time":1200
            }
        }),
    );
    let completed =
        read_until_notification(&mut read, "item/completed", Duration::from_secs(3)).await;
    assert_eq!(completed["params"]["item"]["type"], "commandExecution");
    assert_eq!(completed["params"]["item"]["id"], "call_t1");
    assert_eq!(completed["params"]["item"]["status"], "completed");
    assert_eq!(completed["params"]["item"]["aggregatedOutput"], "a\nb\nc\n");

    drop(write);
    server_task.abort();
}

// ---- shared helpers ----

async fn bring_up_bridge(
    label: &str,
) -> (
    BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
    SseInjector,
    tempfile::TempDir,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let _ = label;
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let sse_injector: SseInjector = Arc::new(Mutex::new(None));
    let base_url = start_fake_opencode(Arc::clone(&seen), Arc::clone(&sse_injector));
    let state_dir = tempfile::TempDir::new().unwrap();
    unsafe {
        std::env::set_var("ALLEYCAT_BRIDGE_STATE_DIR", state_dir.path());
    }
    let bridge = Arc::new(
        OpencodeBridge::new(OpencodeRuntime::external(base_url, "test-token".into()))
            .await
            .unwrap(),
    );
    unsafe {
        std::env::remove_var("ALLEYCAT_BRIDGE_STATE_DIR");
    }
    let (client, server) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(serve_stream(bridge, server));
    let (read, mut write) = tokio::io::split(client);
    let mut read = BufReader::new(read);

    send(
        &mut write,
        1,
        "initialize",
        json!({"clientInfo":{"name":"v3","version":"0"}}),
    )
    .await;
    let _ = read_until_response(&mut read, 1).await;
    send_notification(&mut write, "initialized", json!({})).await;

    // Wait for the SSE consumer to attach.
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

    (read, write, sse_injector, state_dir, server_task)
}

async fn start_thread<W>(
    read: &mut BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    write: &mut W,
    cwd: &str,
) -> String
where
    W: tokio::io::AsyncWrite + Unpin,
{
    send(
        write,
        2,
        "thread/start",
        json!({"cwd":cwd,"model":"openai/gpt-test"}),
    )
    .await;
    let started = read_until_response(read, 2).await;
    started["result"]["thread"]["id"]
        .as_str()
        .unwrap()
        .to_string()
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
        *injector.lock().unwrap() = Some(stream.try_clone().unwrap());
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
            "directory":"/tmp/opencode-v3",
            "title":"V3",
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
