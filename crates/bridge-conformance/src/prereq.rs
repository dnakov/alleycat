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
    Amp { bin: PathBuf },
    Claude { bin: PathBuf },
    Opencode { bin: PathBuf },
    Droid { bin: PathBuf },
    Hermes { bin: PathBuf },
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
        TargetId::Amp => check_amp(),
        TargetId::Claude => check_claude(),
        TargetId::Opencode => check_opencode(),
        TargetId::Droid => check_droid(),
        TargetId::Hermes => check_hermes(),
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

fn check_amp() -> Result<Prereq, SkipReason> {
    let bin = which_or_env("AMP_BRIDGE_AMP_BIN", "amp")
        .ok_or_else(|| SkipReason::Reason("amp not on PATH".into()))?;
    if has_amp_auth() {
        Ok(Prereq::Amp { bin })
    } else {
        Err(SkipReason::Reason(
            "amp auth unavailable: set AMP_API_KEY or run amp login".into(),
        ))
    }
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

fn check_droid() -> Result<Prereq, SkipReason> {
    let bin = which_or_env("DROID_BRIDGE_DROID_BIN", "droid")
        .ok_or_else(|| SkipReason::Reason("droid not on PATH".into()))?;
    if has_factory_auth() {
        Ok(Prereq::Droid { bin })
    } else {
        Err(SkipReason::Reason(
            "droid auth unavailable: set FACTORY_API_KEY or run droid login".into(),
        ))
    }
}

fn check_hermes() -> Result<Prereq, SkipReason> {
    let bin = which_or_env("HERMES_BRIDGE_BIN", "hermes")
        .ok_or_else(|| SkipReason::Reason("hermes not on PATH; set HERMES_BRIDGE_BIN".into()))?;
    Ok(Prereq::Hermes { bin })
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

fn has_factory_auth() -> bool {
    if env::var_os("FACTORY_API_KEY").is_some() {
        return true;
    }
    let Some(home) = env::var_os("HOME") else {
        return false;
    };
    PathBuf::from(home)
        .join(".factory/auth.encrypted")
        .is_file()
}

fn has_amp_auth() -> bool {
    if env::var_os("AMP_API_KEY").is_some() {
        return true;
    }
    let Some(home) = env::var_os("HOME") else {
        return false;
    };
    PathBuf::from(home).join(".amp/oauth").is_dir()
}
