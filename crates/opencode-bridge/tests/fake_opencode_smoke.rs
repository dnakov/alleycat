use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use alleycat_bridge_core::framing::{read_json_line, write_json_line};
use alleycat_bridge_core::server::serve_stream;
use alleycat_bridge_core::{JsonRpcRequest, JsonRpcVersion, RequestId};
use alleycat_opencode_bridge::OpencodeBridge;
use alleycat_opencode_bridge::opencode_proc::OpencodeRuntime;
use serde_json::{Value, json};
use tokio::io::BufReader;

#[tokio::test]
async fn initialize_thread_start_turn_start_smoke() {
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let base_url = start_fake_opencode(Arc::clone(&seen));
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

    let (client, server) = tokio::io::duplex(16 * 1024);
    let server_task = tokio::spawn(serve_stream(bridge, server));
    let (read, mut write) = tokio::io::split(client);
    let mut read = BufReader::new(read);

    send(
        &mut write,
        1,
        "initialize",
        json!({"clientInfo":{"name":"test","version":"0"}}),
    )
    .await;
    let init = read_until_response(&mut read, 1).await;
    assert_eq!(
        init["result"]["userAgent"],
        "alleycat-opencode-bridge/0.1.0"
    );

    send(
        &mut write,
        2,
        "thread/start",
        json!({"cwd":"/tmp/opencode-smoke","model":"openai/gpt-test"}),
    )
    .await;
    let started = read_until_response(&mut read, 2).await;
    let thread_id = started["result"]["thread"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(started["result"]["cwd"], "/tmp/opencode-smoke");

    send(
        &mut write,
        3,
        "turn/start",
        json!({"threadId":thread_id,"input":[{"type":"text","text":"hello"}],"model":"openai/gpt-test"}),
    )
    .await;
    let turn = read_until_response(&mut read, 3).await;
    assert_eq!(turn["result"]["turn"]["status"], "inProgress");

    let paths = seen.lock().unwrap().clone();
    assert!(paths.iter().any(|path| path.starts_with("POST /session?")));
    assert!(
        paths
            .iter()
            .any(|path| path.starts_with("POST /session/ses_1/prompt_async?")),
        "expected prompt_async POST in {paths:?}"
    );

    drop(write);
    server_task.abort();
    unsafe {
        std::env::remove_var("ALLEYCAT_BRIDGE_STATE_DIR");
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

async fn read_until_response<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R, id: i64) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
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

fn start_fake_opencode(seen: Arc<Mutex<Vec<String>>>) -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let seen = Arc::clone(&seen);
            thread::spawn(move || handle_conn(stream, seen));
        }
    });
    format!("http://{addr}")
}

fn handle_conn(mut stream: TcpStream, seen: Arc<Mutex<Vec<String>>>) {
    let mut bytes = Vec::new();
    let mut buf = [0u8; 1024];
    let headers_end = loop {
        let n = stream.read(&mut buf).unwrap_or(0);
        if n == 0 {
            return;
        }
        bytes.extend_from_slice(&buf[..n]);
        if let Some(pos) = find_subsequence(&bytes, b"\r\n\r\n") {
            break pos + 4;
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
        let n = stream.read(&mut buf).unwrap_or(0);
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..n]);
    }
    let req = String::from_utf8_lossy(&bytes);
    let request_line = req.lines().next().unwrap_or("").to_string();
    seen.lock().unwrap().push(request_line.clone());
    if request_line.starts_with("GET /event") {
        // Hold the SSE connection open until the test tears it down.
        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.flush();
        // Park here without sending events; the test exits and shuts the
        // socket. This avoids the consumer reconnect-flapping during the
        // smoke test.
        std::thread::sleep(std::time::Duration::from_secs(60));
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
            "directory":"/tmp/opencode-smoke",
            "title":"Smoke",
            "time":{"created":1000,"updated":1000}
        })
    } else if request_line.starts_with("POST /session/ses_1/message?") {
        json!({
            "info":{"id":"msg_1","role":"assistant"},
            "parts":[{"id":"part_1","type":"text","text":"hello"}]
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
