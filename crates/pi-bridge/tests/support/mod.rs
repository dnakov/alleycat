//! Shared helpers for end-to-end / verification tests.
//!
//! - [`fake_pi_path`] returns the path to the `fake-pi` binary cargo built for
//!   us. Tests pass it to `PiPool::new`.
//! - [`PiHomeFixture`] sets `PI_CODING_AGENT_DIR` to a tempdir and lets tests
//!   seed canned pi JSONL session files for the index scanner.
//! - [`write_script`] writes a `FAKE_PI_SCRIPT` JSONL file from a list of
//!   `serde_json::Value` events.
//!
//! ## Footgun: cargo's in-binary parallelism vs shared env-var fixtures
//!
//! Each `tests/<name>.rs` file becomes its own integration-test binary,
//! and cargo runs different binaries serially **but tests inside the
//! same binary in parallel** (default `--test-threads = num_cpus`).
//! That bites any test that mutates a process-global like `FAKE_PI_SCRIPT`,
//! `FAKE_PI_COMMAND_LOG`, or `PI_CODING_AGENT_DIR`: two `#[tokio::test]`
//! fns in the same file can read/clobber each other's env between
//! `set_var` and the spawn that captures the value.
//!
//! Two patterns to defend against this, both in use:
//!
//! - **Static `tokio::sync::Mutex` per file** (see `v4_approval.rs`'s
//!   `scenario_lock()`): each test takes the lock for the duration of
//!   its env-mutation window. Simplest fix when scenarios just need to
//!   not interleave.
//! - **`PiHomeFixture` Drop guard**: this fixture restores the previous
//!   `PI_CODING_AGENT_DIR` on drop, which is enough when tests don't
//!   *concurrently* hold it but might run sequentially in undefined
//!   order.
//!
//! When in doubt, serialize. The cost of an extra mutex is nothing
//! compared to a flake that only reproduces on a 16-core CI runner.
//!
//! ## fake-pi side-channels
//!
//! - `FAKE_PI_SCRIPT`: JSONL file the fake replays on each `prompt` /
//!   `steer` / `follow_up`. Use [`write_script`] to build one.
//! - `FAKE_PI_COMMAND_LOG`: append-each-command-type log. Set this when
//!   a test needs to assert "the bridge sent pi command X" — see
//!   `v4_approval.rs` for an example. Useful for any V-test that gates
//!   behavior on the bridge's outbound pi traffic (V5 compaction, V7
//!   tool surfacing, future approval variants).

// Each integration test file in `tests/` becomes its own crate and pulls in
// `mod support;`. Helpers used by some files but not others trip dead-code
// warnings in the files that don't reach for them; allow it on the module.
#![allow(dead_code)]

pub mod thread_index_stub;

use std::env;
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tempfile::TempDir;

/// Path to the test-only `fake-pi` binary cargo built alongside the integration
/// tests. The `CARGO_BIN_EXE_<name>` env var is set by cargo for every
/// declared `[[bin]]` in the same crate.
pub fn fake_pi_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake-pi"))
}

/// Tempdir-backed `~/.pi/agent` stand-in. Honors pi's `PI_CODING_AGENT_DIR`
/// env-var override (resolved by [`alleycat_pi_bridge::index::pi_session_scan::pi_agent_dir`])
/// so the bridge points at our seeded sessions instead of the real `$HOME`.
///
/// The fixture restores the previous env var on drop so concurrent tests that
/// don't use the fixture see an unmodified environment. **Do not** use this in
/// tests that run in parallel against a shared global env — wrap them in a
/// once-cell mutex if you need that.
pub struct PiHomeFixture {
    dir: TempDir,
    prev: Option<String>,
}

impl PiHomeFixture {
    pub fn new() -> Self {
        let dir = TempDir::new().expect("tempdir");
        let prev = env::var("PI_CODING_AGENT_DIR").ok();
        // Safety: env mutation is process-global; tests using PiHomeFixture
        // must not run in parallel with other PI_CODING_AGENT_DIR users.
        // Cargo serializes them by default within a single test binary unless
        // `--test-threads` overrides.
        unsafe {
            env::set_var("PI_CODING_AGENT_DIR", dir.path().as_os_str());
        }
        Self { dir, prev }
    }

    pub fn agent_dir(&self) -> &Path {
        self.dir.path()
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.dir.path().join("sessions")
    }

    /// Drop a pi-shaped JSONL session file under `sessions/<encoded_cwd>/<id>.jsonl`.
    /// Caller supplies the entries as raw `serde_json::Value`s (header first).
    /// Returns the absolute path of the file written.
    pub fn seed_session(&self, encoded_cwd: &str, file_stem: &str, entries: &[Value]) -> PathBuf {
        let dir = self.sessions_dir().join(encoded_cwd);
        std::fs::create_dir_all(&dir).expect("create encoded-cwd dir");
        let path = dir.join(format!("{file_stem}.jsonl"));
        let mut f = std::fs::File::create(&path).expect("create session file");
        for entry in entries {
            let line = serde_json::to_string(entry).expect("serialize entry");
            writeln!(f, "{line}").expect("write entry");
        }
        path
    }
}

impl Drop for PiHomeFixture {
    fn drop(&mut self) {
        // Restore previous value so we don't leak state into subsequent tests.
        unsafe {
            match self.prev.take() {
                Some(v) => env::set_var("PI_CODING_AGENT_DIR", OsStr::new(&v)),
                None => env::remove_var("PI_CODING_AGENT_DIR"),
            }
        }
    }
}

/// Persist `events` as a JSONL file the fake-pi binary can load via the
/// `FAKE_PI_SCRIPT` env var. Returns the path so the caller can set the env
/// var (or pass it to a child process).
pub fn write_script(dir: &Path, events: &[Value]) -> PathBuf {
    let path = dir.join("script.jsonl");
    let mut f = std::fs::File::create(&path).expect("create script");
    for event in events {
        let line = serde_json::to_string(event).expect("serialize event");
        writeln!(f, "{line}").expect("write event");
    }
    path
}
