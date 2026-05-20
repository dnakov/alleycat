use std::sync::Arc;

use alleycat_bridge_core::serve_stream;
use alleycat_shell_bridge::ShellBridge;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn spawn_echo_exit_streams_output_and_exit() {
    let bridge = ShellBridge::builder().shell_bin("/bin/sh").build();
    let mut client = spawn_bridge(bridge);

    client
        .request(
            1,
            "shell/spawn",
            json!({
                "shell": "/bin/sh",
                "args": ["-c", "echo hello; exit 0"],
                "size": { "cols": 80, "rows": 24 }
            }),
        )
        .await;

    let response = client.read_until_response(1).await;
    let session_id = response["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let mut saw_hello = false;
    let mut saw_exit = false;
    for _ in 0..32 {
        let frame = client.read_frame().await;
        if frame["method"] == "shell/output" {
            assert_eq!(frame["params"]["session_id"], session_id);
            let data = STANDARD
                .decode(frame["params"]["data_b64"].as_str().unwrap())
                .unwrap();
            if String::from_utf8_lossy(&data).contains("hello") {
                saw_hello = true;
            }
        } else if frame["method"] == "shell/exit" {
            assert_eq!(frame["params"]["session_id"], session_id);
            assert_eq!(frame["params"]["code"], 0);
            saw_exit = true;
        }
        if saw_hello && saw_exit {
            break;
        }
    }

    assert!(saw_hello, "expected shell/output containing hello");
    assert!(saw_exit, "expected shell/exit code 0");
}

struct BridgeClient {
    reader: BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
}

fn spawn_bridge(bridge: Arc<ShellBridge>) -> BridgeClient {
    let (client, server) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        serve_stream(bridge, server).await.unwrap();
    });
    let (reader, writer) = tokio::io::split(client);
    BridgeClient {
        reader: BufReader::new(reader),
        writer,
    }
}

impl BridgeClient {
    async fn request(&mut self, id: i64, method: &str, params: Value) {
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        self.writer
            .write_all(serde_json::to_string(&frame).unwrap().as_bytes())
            .await
            .unwrap();
        self.writer.write_all(b"\n").await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn read_until_response(&mut self, id: i64) -> Value {
        loop {
            let frame = self.read_frame().await;
            if frame["id"] == id {
                return frame;
            }
        }
    }

    async fn read_frame(&mut self) -> Value {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).await.unwrap();
        assert!(n > 0, "bridge stream closed");
        serde_json::from_str(&line).unwrap()
    }
}
