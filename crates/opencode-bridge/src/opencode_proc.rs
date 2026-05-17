use std::ffi::OsStr;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command as StdCommand;
use std::process::Stdio;
use std::time::{Duration, Instant};

use alleycat_bridge_core::{LaunchEnvironment, LaunchEnvironmentResolver};
use rand::RngCore;
use tokio::process::{Child, Command as TokioCommand};

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
        let cwd = std::env::current_dir().ok();
        let launch_env = LaunchEnvironmentResolver::default()
            .resolve(cwd.as_deref())
            .await;

        if let Some(base_url) = env_string(&launch_env, "OPENCODE_BRIDGE_BACKEND_URL") {
            let auth_token =
                env_string(&launch_env, "OPENCODE_BRIDGE_AUTH_TOKEN").unwrap_or_default();
            return Ok(Self {
                base_url,
                auth_token,
                _child: None,
            });
        }

        // The daemon writes host-configured opencode.bin into its own process
        // environment just before lazy bridge construction. Keep that explicit
        // config override above shell/mise/direnv ambient values.
        let configured_bin = std::env::var("OPENCODE_BRIDGE_BIN").ok();
        let bin = resolve_opencode_bin(&launch_env, configured_bin.as_deref());
        let port = match env_string(&launch_env, "OPENCODE_BRIDGE_PORT").as_deref() {
            Some("auto") | None => pick_port()?,
            Some(value) => value.parse::<u16>()?,
        };
        // `--auth-token` was removed from `opencode serve` in 1.3.x and
        // passing it makes the binary print usage and exit immediately. Only
        // forward an explicit override; otherwise leave it off and treat the
        // server as unauthenticated (`OpencodeClient` skips the query param
        // when `auth_token` is empty).
        let explicit_auth_token =
            match env_string(&launch_env, "OPENCODE_BRIDGE_AUTH_TOKEN").as_deref() {
                Some("auto") | Some("") | None => None,
                Some(value) => Some(value.to_string()),
            };
        let auth_token = explicit_auth_token.clone().unwrap_or_default();
        let extra_args = env_string(&launch_env, "OPENCODE_BRIDGE_EXTRA_ARGS")
            .map(|raw| {
                raw.split('\u{1f}')
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec!["serve".to_string()]);

        let mut command = TokioCommand::new(bin);
        command
            .env_clear()
            .envs(launch_env.clone().into_pairs())
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

fn resolve_opencode_bin(env: &LaunchEnvironment, configured_bin: Option<&str>) -> PathBuf {
    let env_configured = env_string(env, "OPENCODE_BRIDGE_BIN");
    if let Some(raw) = configured_bin.or(env_configured.as_deref()) {
        let bin = raw.trim();
        if !bin.is_empty() && bin != "opencode" {
            return PathBuf::from(bin);
        }
    }

    if let Some(path) = env.find_on_path("opencode")
        && command_looks_usable(&path, env)
    {
        return path;
    }

    for candidate in fallback_opencode_bins(env) {
        if command_looks_usable(&candidate, env) {
            return candidate;
        }
    }

    PathBuf::from("opencode")
}

fn fallback_opencode_bins(env: &LaunchEnvironment) -> Vec<PathBuf> {
    let mut bins = Vec::new();
    if let Some(home) = env.get("HOME") {
        bins.push(PathBuf::from(home).join(".opencode/bin/opencode"));
    }
    bins.push(PathBuf::from("/opt/homebrew/bin/opencode"));
    bins.push(PathBuf::from("/usr/local/bin/opencode"));
    bins
}

fn command_looks_usable(bin: impl AsRef<OsStr>, env: &LaunchEnvironment) -> bool {
    let mut command = StdCommand::new(bin);
    command
        .env_clear()
        .envs(env.clone().into_pairs())
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn env_string(env: &LaunchEnvironment, key: &str) -> Option<String> {
    env.get(key)
        .and_then(OsStr::to_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
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
