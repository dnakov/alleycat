//! Per-target prerequisite probes. Each test asks `check(target)` before
//! spawning anything; if the prereq isn't met we print a skip reason and
//! return `None` so the test passes silently. Same shape as
//! `crates/pi-bridge/tests/v8_live_pi.rs::check_prereqs`.

use std::env;
use std::path::PathBuf;

use crate::TargetId;

#[derive(Debug)]
pub enum Prereq {
    Codex { bin: PathBuf },
    Pi { bin: PathBuf },
    Claude { bin: PathBuf },
    Opencode { bin: PathBuf },
}

#[derive(Debug)]
pub enum SkipReason {
    Reason(String),
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkipReason::Reason(s) => f.write_str(s),
        }
    }
}

pub async fn check(target: TargetId) -> Result<Prereq, SkipReason> {
    match target {
        TargetId::Codex => check_codex(),
        TargetId::Pi => check_pi(),
        TargetId::Claude => check_claude(),
        TargetId::Opencode => check_opencode(),
    }
}

fn check_codex() -> Result<Prereq, SkipReason> {
    // Spawn `codex app-server` ourselves over stdio — no port to fight over
    // and the wire shape is the same JSON-RPC the bridges already speak.
    let bin = which_or_env("CODEX_BIN", "codex")
        .ok_or_else(|| SkipReason::Reason("codex not on PATH".into()))?;
    Ok(Prereq::Codex { bin })
}

fn check_pi() -> Result<Prereq, SkipReason> {
    let bin = which_or_env("PI_BRIDGE_PI_BIN", "pi-coding-agent")
        .ok_or_else(|| SkipReason::Reason("pi-coding-agent not on PATH".into()))?;
    Ok(Prereq::Pi { bin })
}

fn check_claude() -> Result<Prereq, SkipReason> {
    let bin = which_or_env("CLAUDE_BRIDGE_CLAUDE_BIN", "claude")
        .ok_or_else(|| SkipReason::Reason("claude not on PATH".into()))?;
    Ok(Prereq::Claude { bin })
}

fn check_opencode() -> Result<Prereq, SkipReason> {
    let bin = which_or_env("OPENCODE_BRIDGE_BIN", "opencode")
        .ok_or_else(|| SkipReason::Reason("opencode not on PATH".into()))?;
    Ok(Prereq::Opencode { bin })
}

fn which_or_env(env_var: &str, bin_name: &str) -> Option<PathBuf> {
    if let Some(p) = env::var_os(env_var) {
        if !p.is_empty() {
            let path = PathBuf::from(p);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    which::which(bin_name).ok()
}
