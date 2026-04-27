use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde_json::Value;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, warn};

use crate::opencode_client::OpencodeClient;

const BROADCAST_CAPACITY: usize = 1024;
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);
const BACKOFF_INITIAL: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// A long-running SSE consumer that maintains a single `GET /event` stream to
/// opencode and fans out parsed events to any number of subscribers via a
/// `tokio::sync::broadcast` channel. Reconnects with exponential backoff on
/// stream error and on heartbeat timeout.
#[derive(Clone)]
pub struct SseConsumer {
    tx: broadcast::Sender<Arc<Value>>,
    _task: Arc<JoinHandle<()>>,
}

impl SseConsumer {
    /// Spawn the background consumer task. The first connection attempt happens
    /// asynchronously; callers may `subscribe()` immediately even before the
    /// stream is open.
    pub fn spawn(client: OpencodeClient) -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        let tx_for_task = tx.clone();
        let task = tokio::spawn(async move {
            run_consumer(client, tx_for_task).await;
        });
        Self {
            tx,
            _task: Arc::new(task),
        }
    }

    /// Subscribe to the live event stream. Each subscriber receives every event
    /// the consumer parses from the moment of subscription onward.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Value>> {
        self.tx.subscribe()
    }

    /// Number of currently active subscribers (primarily for tests).
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

async fn run_consumer(client: OpencodeClient, tx: broadcast::Sender<Arc<Value>>) {
    let mut backoff = BACKOFF_INITIAL;
    loop {
        match run_once(&client, &tx).await {
            Ok(()) => {
                debug!("opencode SSE stream closed cleanly; reconnecting");
                backoff = BACKOFF_INITIAL;
            }
            Err(error) => {
                warn!("opencode SSE stream errored: {error:#}; reconnecting in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
        }
        tokio::time::sleep(backoff).await;
    }
}

async fn run_once(
    client: &OpencodeClient,
    tx: &broadcast::Sender<Arc<Value>>,
) -> anyhow::Result<()> {
    let resp = client.raw_get("/event").await?;
    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut last_event = Instant::now();
    loop {
        let chunk = tokio::select! {
            chunk = stream.next() => chunk,
            _ = tokio::time::sleep_until(last_event + HEARTBEAT_TIMEOUT) => {
                anyhow::bail!("no SSE event/heartbeat in {HEARTBEAT_TIMEOUT:?}");
            }
        };
        let Some(chunk) = chunk else {
            return Ok(());
        };
        let chunk = chunk?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(idx) = buffer.find("\n\n") {
            let frame = buffer[..idx].to_string();
            buffer = buffer[idx + 2..].to_string();
            for line in frame.lines() {
                if let Some(data) = line.strip_prefix("data:")
                    && let Ok(value) = serde_json::from_str::<Value>(data.trim())
                {
                    last_event = Instant::now();
                    // broadcast::send() returns Err only when no receivers; that's fine.
                    let _ = tx.send(Arc::new(value));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::Duration;

    /// Spawn a TCP listener that serves SSE on `GET /event`. `frames_per_conn`
    /// is sent on each new connection (one entry per connection); when the list
    /// is exhausted the listener stops accepting. Returns the base URL plus a
    /// counter that records how many `/event` requests were served.
    fn start_fake_sse(
        frames_per_conn: Vec<Vec<&'static str>>,
    ) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_thread = Arc::clone(&counter);
        thread::spawn(move || {
            let mut iter = frames_per_conn.into_iter();
            for stream in listener.incoming().flatten() {
                let frames = match iter.next() {
                    Some(f) => f,
                    None => break,
                };
                counter_thread.fetch_add(1, Ordering::SeqCst);
                thread::spawn(move || serve_sse_conn(stream, frames));
            }
        });
        (format!("http://{addr}"), counter)
    }

    fn serve_sse_conn(mut stream: std::net::TcpStream, frames: Vec<&'static str>) {
        let mut buf = [0u8; 1024];
        // Drain headers (best-effort).
        let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
        loop {
            match stream.read(&mut buf) {
                Ok(0) => return,
                Ok(_) => {
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
        if stream.write_all(head.as_bytes()).is_err() {
            return;
        }
        for frame in frames {
            let payload = format!("data: {frame}\n\n");
            if stream.write_all(payload.as_bytes()).is_err() {
                return;
            }
            let _ = stream.flush();
            // Small pause so the receiver definitely processes between chunks.
            thread::sleep(Duration::from_millis(25));
        }
        // Close the connection by dropping the stream.
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }

    #[tokio::test]
    async fn fan_out_delivers_event_to_each_subscriber_exactly_once() {
        let (base_url, _) = start_fake_sse(vec![
            vec![r#"{"type":"server.connected","properties":{}}"#],
        ]);
        let client = OpencodeClient::new(base_url, String::new());
        let consumer = SseConsumer::spawn(client);
        let mut a = consumer.subscribe();
        let mut b = consumer.subscribe();

        let event_a = tokio::time::timeout(Duration::from_secs(2), a.recv())
            .await
            .expect("subscriber a timed out")
            .expect("subscriber a recv");
        let event_b = tokio::time::timeout(Duration::from_secs(2), b.recv())
            .await
            .expect("subscriber b timed out")
            .expect("subscriber b recv");

        assert_eq!(event_a.get("type").and_then(Value::as_str), Some("server.connected"));
        assert_eq!(event_b.get("type").and_then(Value::as_str), Some("server.connected"));

        // Each subscriber received exactly one event for that frame.
        assert!(a.try_recv().is_err());
        assert!(b.try_recv().is_err());
    }

    #[tokio::test]
    async fn reconnects_after_dropped_connection() {
        let (base_url, counter) = start_fake_sse(vec![
            vec![r#"{"type":"server.connected","properties":{}}"#],
            vec![r#"{"type":"session.idle","properties":{"sessionID":"ses_1"}}"#],
        ]);
        let client = OpencodeClient::new(base_url, String::new());
        let consumer = SseConsumer::spawn(client);
        let mut rx = consumer.subscribe();

        let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("first event timed out")
            .expect("first event");
        assert_eq!(first.get("type").and_then(Value::as_str), Some("server.connected"));

        let second = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("second event (after reconnect) timed out")
            .expect("second event");
        assert_eq!(second.get("type").and_then(Value::as_str), Some("session.idle"));
        assert!(counter.load(Ordering::SeqCst) >= 2, "expected at least two /event connections, got {}", counter.load(Ordering::SeqCst));
    }
}
