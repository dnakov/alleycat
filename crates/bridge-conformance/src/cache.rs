//! Persistent state for the conformance harness so test runs reuse one
//! thread per target instead of creating a fresh thread (and a fresh
//! tempdir cwd) every time. The user's pi/claude/opencode "thread list"
//! UI is shared with the rest of their workflow; without this cache,
//! every conformance run would add another row to it.
//!
//! Two artifacts:
//! - `~/.cache/alleycat-bridge-conformance/threads.json` — map of
//!   `target_label -> thread_id`. Created lazily on first save.
//! - `~/.cache/alleycat-bridge-conformance/cwd/` — the stable working
//!   directory passed as `thread/start.cwd`. Created lazily by the test
//!   harness; never deleted.
//!
//! Cache misses (no entry, parse failure, missing HOME) are silent — the
//! caller falls back to creating a fresh thread. This keeps the harness
//! usable on machines where `$HOME` isn't writable (CI sandboxes etc.).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Map, Value};

use crate::TargetId;

/// Resolve the directory holding both the thread cache and the stable cwd.
/// Returns `None` only when `$HOME` is unset; in that case the caller falls
/// back to ephemeral state.
pub fn cache_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let mut p = PathBuf::from(home);
    p.push(".cache");
    p.push("alleycat-bridge-conformance");
    Some(p)
}

/// Path to `threads.json`. Doesn't have to exist — `load_thread_id` returns
/// None on missing/parse-failed.
fn threads_path() -> Option<PathBuf> {
    cache_root().map(|p| p.join("threads.json"))
}

/// Stable cwd directory passed as `thread/start.cwd` for every target. The
/// directory is created on first call so the same path is reused forever.
pub fn stable_cwd() -> Result<PathBuf> {
    let root = cache_root().context("HOME not set; cannot resolve cwd cache")?;
    let cwd = root.join("cwd");
    std::fs::create_dir_all(&cwd)
        .with_context(|| format!("create stable cwd at {}", cwd.display()))?;
    Ok(cwd)
}

pub fn load_thread_id(target: TargetId) -> Option<String> {
    let path = threads_path()?;
    let data = std::fs::read_to_string(&path).ok()?;
    let map: Map<String, Value> = serde_json::from_str(&data).ok()?;
    map.get(target.label())
        .and_then(Value::as_str)
        .map(str::to_string)
}

pub fn save_thread_id(target: TargetId, thread_id: &str) -> Result<()> {
    let path = threads_path().context("HOME not set; cannot save thread id")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create cache dir {}", parent.display()))?;
    }
    let mut map = read_map(&path).unwrap_or_default();
    map.insert(
        target.label().to_string(),
        Value::String(thread_id.to_string()),
    );
    let body = serde_json::to_string_pretty(&Value::Object(map))?;
    std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn read_map(path: &Path) -> Option<Map<String, Value>> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

/// Forget the saved id for `target`. Used when a `thread/resume` against the
/// cached id fails — keeping the stale id around just causes every future run
/// to fail the same way.
pub fn clear_thread_id(target: TargetId) -> Result<()> {
    let Some(path) = threads_path() else {
        return Ok(());
    };
    let Some(mut map) = read_map(&path) else {
        return Ok(());
    };
    if map.remove(target.label()).is_none() {
        return Ok(());
    }
    let body = serde_json::to_string_pretty(&Value::Object(map))?;
    std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}
