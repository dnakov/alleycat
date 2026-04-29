//! T4 acceptance: `message.part.delta` is routed by the cached `PartKind`,
//! not by the field name alone. Drives several (part.updated, part.delta)
//! pairs and asserts each delta lands on the matching codex notification
//! topic — and that the previously-broken misroute (any non-text field →
//! `item/commandExecution/outputDelta`) no longer happens for reasoning,
//! MCP, or apply_patch parts.

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
async fn reasoning_text_delta_routes_to_reasoning_text_delta_topic() {
    let (mut read, mut write, sse_injector, _state_dir, server_task) = bring_up_bridge().await;
    let thread_id = start_thread(&mut read, &mut write, "/tmp/opencode-v4").await;
    start_active_turn(&mut read, &mut write, &thread_id).await;

    // Cache the part kind first.
    inject_sse(
        &sse_injector,
        json!({
            "type":"message.part.updated",
            "properties":{
                "sessionID":"ses_1",
                "part":{"id":"r1","messageID":"m1","sessionID":"ses_1","type":"reasoning","text":"","time":{"start":1}},
                "time":1
            }
        }),
    );
    // Then a delta. With the old (T3) routing this would have produced
    // `item/agentMessage/delta` because field=="text"; with T4 it must go to
    // `item/reasoning/textDelta`.
    inject_sse(
        &sse_injector,
        json!({
            "type":"message.part.delta",
            "properties":{
                "sessionID":"ses_1",
                "messageID":"m1",
                "partID":"r1",
                "field":"text",
                "delta":"hmm"
            }
        }),
    );
    let notif = read_until_one_of(
        &mut read,
        &[
            "item/reasoning/textDelta",
            "item/agentMessage/delta",
            "item/commandExecution/outputDelta",
        ],
        Duration::from_secs(3),
    )
    .await;
    assert_eq!(
        notif["method"], "item/reasoning/textDelta",
        "reasoning deltas must use the reasoning topic; misroute regression detected"
    );
    assert_eq!(notif["params"]["delta"], "hmm");
    assert_eq!(notif["params"]["contentIndex"], 0);
    assert_eq!(notif["params"]["threadId"], thread_id);

    drop(write);
    server_task.abort();
}

#[tokio::test]
async fn mcp_tool_output_delta_routes_to_mcp_progress() {
    let (mut read, mut write, sse_injector, _state_dir, server_task) = bring_up_bridge().await;
    let thread_id = start_thread(&mut read, &mut write, "/tmp/opencode-v4-mcp").await;
    start_active_turn(&mut read, &mut write, &thread_id).await;

    // Register an MCP tool part. tool name uses opencode's `<server>__<name>`
    // convention so classify_part returns ToolMcp.
    inject_sse(
        &sse_injector,
        json!({
            "type":"message.part.updated",
            "properties":{
                "sessionID":"ses_1",
                "part":{
                    "id":"mcp1","callID":"c_mcp1","messageID":"m2","sessionID":"ses_1",
                    "type":"tool","tool":"github__create_issue",
                    "state":{"status":"running","input":{},"time":{"start":2}}
                },
                "time":2
            }
        }),
    );
    // Drain the item/started so subsequent reads land on the delta we care about.
    let _ = read_until_notification(&mut read, "item/started", Duration::from_secs(3)).await;

    inject_sse(
        &sse_injector,
        json!({
            "type":"message.part.delta",
            "properties":{
                "sessionID":"ses_1",
                "messageID":"m2",
                "partID":"mcp1",
                "field":"output",
                "delta":"creating issue..."
            }
        }),
    );
    let notif = read_until_one_of(
        &mut read,
        &[
            "item/mcpToolCall/progress",
            "item/commandExecution/outputDelta",
        ],
        Duration::from_secs(3),
    )
    .await;
    assert_eq!(notif["method"], "item/mcpToolCall/progress");
    assert_eq!(notif["params"]["message"], "creating issue...");

    drop(write);
    server_task.abort();
}

#[tokio::test]
async fn bash_tool_output_delta_still_routes_to_command_execution() {
    let (mut read, mut write, sse_injector, _state_dir, server_task) = bring_up_bridge().await;
    let thread_id = start_thread(&mut read, &mut write, "/tmp/opencode-v4-bash").await;
    start_active_turn(&mut read, &mut write, &thread_id).await;

    inject_sse(
        &sse_injector,
        json!({
            "type":"message.part.updated",
            "properties":{
                "sessionID":"ses_1",
                "part":{
                    "id":"b1","callID":"c_b1","messageID":"m3","sessionID":"ses_1",
                    "type":"tool","tool":"bash",
                    "state":{"status":"running","input":{"command":"sleep 1"},"time":{"start":3}}
                },
                "time":3
            }
        }),
    );
    let _ = read_until_notification(&mut read, "item/started", Duration::from_secs(3)).await;

    inject_sse(
        &sse_injector,
        json!({
            "type":"message.part.delta",
            "properties":{
                "sessionID":"ses_1",
                "messageID":"m3",
                "partID":"b1",
                "field":"output",
                "delta":"hello\n"
            }
        }),
    );
    let notif = read_until_notification(
        &mut read,
        "item/commandExecution/outputDelta",
        Duration::from_secs(3),
    )
    .await;
    assert_eq!(notif["params"]["delta"], "hello\n");

    drop(write);
    server_task.abort();
}

#[tokio::test]
async fn apply_patch_tool_output_delta_routes_to_file_change() {
    let (mut read, mut write, sse_injector, _state_dir, server_task) = bring_up_bridge().await;
    let thread_id = start_thread(&mut read, &mut write, "/tmp/opencode-v4-patch").await;
    start_active_turn(&mut read, &mut write, &thread_id).await;

    inject_sse(
        &sse_injector,
        json!({
            "type":"message.part.updated",
            "properties":{
                "sessionID":"ses_1",
                "part":{
                    "id":"ap1","callID":"c_ap1","messageID":"m4","sessionID":"ses_1",
                    "type":"tool","tool":"apply_patch",
                    "state":{"status":"running","input":{},"time":{"start":4}}
                },
                "time":4
            }
        }),
    );
    let _ = read_until_notification(&mut read, "item/started", Duration::from_secs(3)).await;

    inject_sse(
        &sse_injector,
        json!({
            "type":"message.part.delta",
            "properties":{
                "sessionID":"ses_1",
                "messageID":"m4",
                "partID":"ap1",
                "field":"output",
                "delta":"applying patch..."
            }
        }),
    );
    let notif = read_until_notification(
        &mut read,
        "item/fileChange/outputDelta",
        Duration::from_secs(3),
    )
    .await;
    assert_eq!(notif["params"]["delta"], "applying patch...");

    drop(write);
    server_task.abort();
}

// ---- shared helpers ----

async fn bring_up_bridge() -> (
    BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
    SseInjector,
    tempfile::TempDir,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
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
        json!({"clientInfo":{"name":"v4","version":"0"}}),
    )
    .await;
    let _ = read_until_response(&mut read, 1).await;
    send_notification(&mut write, "initialized", json!({})).await;

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

async fn start_active_turn<W>(
    read: &mut BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    write: &mut W,
    thread_id: &str,
) where
    W: tokio::io::AsyncWrite + Unpin,
{
    send(
        write,
        3,
        "turn/start",
        json!({"threadId":thread_id,"input":[{"type":"text","text":"go"}]}),
    )
    .await;
    let _ = read_until_response(read, 3).await;
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

/// Read until any of the named notifications arrives, asserting that the
/// FIRST matching notification matches expectations. Used so a misroute test
/// can fail fast instead of timing out: e.g. wait for either the correct
/// `item/reasoning/textDelta` or the regression `item/commandExecution/outputDelta`.
async fn read_until_one_of<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    methods: &[&str],
    timeout: Duration,
) -> Value {
    tokio::time::timeout(timeout, async {
        loop {
            let value: Value = read_json_line(reader).await.unwrap().unwrap();
            if let Some(m) = value.get("method").and_then(Value::as_str)
                && methods.contains(&m)
            {
                return value;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("none of {methods:?} seen within {timeout:?}"))
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
            "directory":"/tmp/opencode-v4",
            "title":"V4",
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
