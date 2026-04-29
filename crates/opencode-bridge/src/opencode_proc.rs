use std::net::TcpListener;
use std::process::Stdio;
use std::time::{Duration, Instant};

use rand::RngCore;
use tokio::process::{Child, Command};

pub struct OpencodeRuntime {
    pub base_url: String,
    pub auth_token: String,
    _child: Option<Child>,
}

impl OpencodeRuntime {
    pub fn external(base_url: String, auth_token: String) -> Self {
        Self {
            base_url,
            auth_token,
            _child: None,
        }
    }

    pub async fn start_from_env() -> anyhow::Result<Self> {
        if let Ok(base_url) = std::env::var("OPENCODE_BRIDGE_BACKEND_URL") {
            let auth_token = std::env::var("OPENCODE_BRIDGE_AUTH_TOKEN").unwrap_or_default();
            return Ok(Self {
                base_url,
                auth_token,
                _child: None,
            });
        }

        let bin = std::env::var("OPENCODE_BRIDGE_BIN").unwrap_or_else(|_| "opencode".to_string());
        let port = match std::env::var("OPENCODE_BRIDGE_PORT").as_deref() {
            Ok("auto") | Err(_) => pick_port()?,
            Ok(value) => value.parse::<u16>()?,
        };
        // `--auth-token` was removed from `opencode serve` in 1.3.x and
        // passing it makes the binary print usage and exit immediately. Only
        // forward an explicit override; otherwise leave it off and treat the
        // server as unauthenticated (`OpencodeClient` skips the query param
        // when `auth_token` is empty).
        let explicit_auth_token = match std::env::var("OPENCODE_BRIDGE_AUTH_TOKEN").as_deref() {
            Ok("auto") | Ok("") | Err(_) => None,
            Ok(value) => Some(value.to_string()),
        };
        let auth_token = explicit_auth_token.clone().unwrap_or_default();
        let extra_args = std::env::var("OPENCODE_BRIDGE_EXTRA_ARGS")
            .ok()
            .map(|raw| {
                raw.split('\u{1f}')
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec!["serve".to_string()]);

        let mut command = Command::new(bin);
        command
            .args(extra_args)
            .arg(format!("--port={port}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        if let Some(token) = explicit_auth_token.as_deref() {
            command.arg(format!("--auth-token={token}"));
        }
        let child = command.spawn()?;
        let base_url = format!("http://127.0.0.1:{port}");
        wait_until_healthy(&base_url, READINESS_TIMEOUT).await?;
        Ok(Self {
            base_url,
            auth_token,
            _child: Some(child),
        })
    }
}

const READINESS_TIMEOUT: Duration = Duration::from_secs(10);
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Poll `GET {base_url}/global/health` until it returns `{healthy:true}` or
/// `timeout` elapses. Replaces the previous fixed 300ms sleep with a
/// race-free readiness gate.
async fn wait_until_healthy(base_url: &str, timeout: Duration) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let url = format!("{}/global/health", base_url.trim_end_matches('/'));
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(resp) = client.get(&url).send().await
            && resp.status().is_success()
            && let Ok(body) = resp.json::<serde_json::Value>().await
            && body.get("healthy").and_then(serde_json::Value::as_bool) == Some(true)
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(anyhow::anyhow!(
                "opencode did not report healthy at {url} within {timeout:?}"
            ));
        }
        tokio::time::sleep(READINESS_POLL_INTERVAL).await;
    }
}

fn pick_port() -> anyhow::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

#[allow(dead_code)]
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_constructor_stores_fields_and_spawns_no_child() {
        let runtime = OpencodeRuntime::external(
            "http://example.test:1234".to_string(),
            "tok-abc".to_string(),
        );
        assert_eq!(runtime.base_url, "http://example.test:1234");
        assert_eq!(runtime.auth_token, "tok-abc");
        assert!(runtime._child.is_none());
    }
}
