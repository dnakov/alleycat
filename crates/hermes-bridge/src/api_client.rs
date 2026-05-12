//! Hermes gateway API client.
//!
//! HTTP client for the Hermes gateway REST API:
//!   - `GET  /health` — liveness probe
//!   - `POST /v1/runs` — start a new run
//!   - `GET  /v1/runs/{run_id}/events` — stream SSE events
//!   - `POST /v1/runs/{run_id}/stop` — interrupt a running turn

use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

/// Default Hermes gateway address (loopback only).
pub const DEFAULT_API_BASE: &str = "http://127.0.0.1:8642";

/// Environment variable checked for the API key.
/// The key is used server-side only and MUST NOT be sent to mobile clients.
pub const DEFAULT_API_KEY_ENV: &str = "HERMES_API_KEY";

/// Response from `GET /health`.
#[derive(Debug, Deserialize)]
pub struct HealthResponse {
    pub status: String,
}

/// Request body for `POST /v1/runs`.
#[derive(Debug, Serialize)]
pub struct CreateRunRequest {
    /// The prompt or message to send (stock Hermes API field).
    pub input: String,
    /// Optional session ID for thread continuity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Optional working directory for the run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Optional model override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Response from `POST /v1/runs`.
#[derive(Debug, Deserialize)]
pub struct CreateRunResponse {
    #[serde(alias = "runId")]
    pub run_id: String,
    #[serde(default, alias = "sessionId")]
    pub session_id: Option<String>,
}

/// A single Hermes SSE frame parsed from `event:` + `data:` lines.
#[derive(Debug, Clone, PartialEq)]
pub struct HermesEvent {
    pub event: String,
    pub data: Value,
}

impl HermesEvent {
    pub fn message_delta(&self) -> Option<String> {
        if self.event != "message.delta" {
            return None;
        }
        self.data
            .get("delta")
            .or_else(|| self.data.get("text"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    }

    pub fn is_terminal_success(&self) -> bool {
        self.event == "run.completed"
    }

    pub fn terminal_error(&self) -> Option<String> {
        match self.event.as_str() {
            "run.failed" | "run.cancelled" => Some(
                self.data
                    .get("message")
                    .or_else(|| self.data.get("error"))
                    .and_then(Value::as_str)
                    .unwrap_or(self.event.as_str())
                    .to_string(),
            ),
            _ => None,
        }
    }
}

/// Hermes API client — talks to the gateway REST + SSE endpoints.
pub struct HermesApiClient {
    client: Client,
    base_url: String,
    api_key: Option<String>,
}

impl HermesApiClient {
    pub fn new(base_url: &str, api_key: Option<String>) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .expect("reqwest client should build");
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
        }
    }

    /// Check if the Hermes gateway is reachable and healthy.
    pub async fn health(&self) -> Result<HealthResponse> {
        let url = format!("{}/health", self.base_url);
        let mut req = self.client.get(&url);
        if let Some(ref key) = self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("health probe to {}", url))?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("health check failed: {}", status);
        }
        let health: HealthResponse = resp
            .json()
            .await
            .with_context(|| "parsing health response")?;
        Ok(health)
    }

    /// Start a new run (turn) on the Hermes gateway.
    pub async fn create_run(&self, req: CreateRunRequest) -> Result<CreateRunResponse> {
        let url = format!("{}/v1/runs", self.base_url);
        let mut request = self.client.post(&url);
        if let Some(ref key) = self.api_key {
            request = request.bearer_auth(key);
        }
        let resp = request
            .json(&req)
            .send()
            .await
            .with_context(|| "POST /v1/runs")?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("create run failed: {}", status);
        }
        let run: CreateRunResponse = resp
            .json()
            .await
            .with_context(|| "parsing create run response")?;
        Ok(run)
    }

    /// Return a streaming `reqwest::Response` for `/v1/runs/{run_id}/events`.
    /// The caller is responsible for parsing the SSE stream.
    pub async fn events_stream(&self, run_id: &str) -> Result<reqwest::Response> {
        let url = format!("{}/v1/runs/{}/events", self.base_url, run_id);
        let mut req = self.client.get(&url);
        if let Some(ref key) = self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("GET /v1/runs/{}/events", run_id))?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("events stream request failed: {}", status);
        }
        Ok(resp)
    }

    /// Interrupt a running turn.
    pub async fn stop_run(&self, run_id: &str) -> Result<()> {
        let url = format!("{}/v1/runs/{}/stop", self.base_url, run_id);
        let mut req = self.client.post(&url);
        if let Some(ref key) = self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .await
            .with_context(|| "POST /v1/runs/{id}/stop")?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("stop run failed: {}", status);
        }
        Ok(())
    }

    /// Resolve a pending tool approval for a running turn.
    pub async fn approve_run_once(&self, run_id: &str) -> Result<()> {
        let url = format!("{}/v1/runs/{}/approval", self.base_url, run_id);
        let mut req = self.client.post(&url);
        if let Some(ref key) = self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .json(&serde_json::json!({ "choice": "once" }))
            .send()
            .await
            .with_context(|| "POST /v1/runs/{id}/approval")?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("approve run failed: {}", status);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_run_request_uses_stock_input_field() {
        let req = CreateRunRequest {
            input: "hello".to_string(),
            session_id: Some("thread-1".to_string()),
            cwd: Some("/tmp".to_string()),
            model: None,
        };
        let value = serde_json::to_value(req).unwrap();
        assert_eq!(
            value.get("input"),
            Some(&Value::String("hello".to_string()))
        );
        assert!(value.get("prompt").is_none());
        assert_eq!(
            value.get("session_id"),
            Some(&Value::String("thread-1".to_string()))
        );
        assert!(value.get("sessionId").is_none());
    }

    #[tokio::test]
    async fn fake_hermes_api_server_health_run_events_stop() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..4 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 8192];
                let n = socket.read(&mut buf).await.unwrap();
                let req = String::from_utf8_lossy(&buf[..n]);
                let (status, content_type, body) = if req.starts_with("GET /health ") {
                    (
                        "200 OK",
                        "application/json",
                        "{\"status\":\"ok\"}".to_string(),
                    )
                } else if req.starts_with("POST /v1/runs ") {
                    assert!(req.contains("\"input\":\"hello\""));
                    assert!(!req.contains("\"prompt\""));
                    (
                        "200 OK",
                        "application/json",
                        "{\"run_id\":\"run-1\",\"status\":\"running\"}".to_string(),
                    )
                } else if req.starts_with("GET /v1/runs/run-1/events ") {
                    (
                        "200 OK",
                        "text/event-stream",
                        "event: message.delta\ndata: {\"delta\":\"hi\"}\n\nevent: run.completed\ndata: {\"status\":\"completed\"}\n\n".to_string(),
                    )
                } else if req.starts_with("POST /v1/runs/run-1/stop ") {
                    ("200 OK", "application/json", "{}".to_string())
                } else {
                    ("404 Not Found", "text/plain", "not found".to_string())
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(resp.as_bytes()).await.unwrap();
            }
        });

        let client = HermesApiClient::new(&format!("http://{addr}"), None);
        assert_eq!(client.health().await.unwrap().status, "ok");
        let run = client
            .create_run(CreateRunRequest {
                input: "hello".to_string(),
                session_id: Some("session-1".to_string()),
                cwd: None,
                model: None,
            })
            .await
            .unwrap();
        assert_eq!(run.run_id, "run-1");
        let body = client
            .events_stream("run-1")
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let events = crate::sse::parse_sse_frames(&body);
        assert_eq!(events[0].message_delta().as_deref(), Some("hi"));
        client.stop_run("run-1").await.unwrap();
        server.await.unwrap();
    }
}
