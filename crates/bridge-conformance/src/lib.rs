//! Cross-implementation conformance harness for the codex app-server JSON-RPC
//! protocol.
//!
//! Drives one canonical scenario (`scenario::run`) through every reachable
//! target — real `codex app-server` over TCP plus each of the alleycat bridges
//! over their native transport — and records a [`Transcript`] of every frame
//! emitted on the wire. The recorded transcripts are then validated by
//! [`schema`] (do they deserialize cleanly into the typed `codex-proto`
//! structs?) and compared by [`diff`] (do bridges emit the same shapes and
//! the same notification sequence as codex?).
//!
//! The whole suite is gated behind `#[ignore]` because each target needs a
//! real backend (`pi-coding-agent`, `claude`, `opencode`, `codex app-server`).
//! See `tests/conformance.rs` for the entry points and `prereq` for the
//! skip-on-missing logic.

pub mod cache;
pub mod diff;
pub mod prereq;
pub mod scenario;
pub mod schema;
pub mod streaming;
pub mod targets;
pub mod transport;
pub mod upstream_schema;

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One of the four implementations the harness can run against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetId {
    /// Real `codex app-server` over TCP — the canonical reference.
    Codex,
    /// `alleycat-pi-bridge` over stdio (`pi-coding-agent` backend).
    Pi,
    /// `alleycat-claude-bridge` over stdio (`claude -p` backend).
    Claude,
    /// `alleycat-opencode-bridge` over Unix socket (`opencode serve` backend).
    Opencode,
}

impl TargetId {
    pub const ALL: &'static [TargetId] = &[
        TargetId::Codex,
        TargetId::Pi,
        TargetId::Claude,
        TargetId::Opencode,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TargetId::Codex => "codex",
            TargetId::Pi => "pi",
            TargetId::Claude => "claude",
            TargetId::Opencode => "opencode",
        }
    }
}

impl fmt::Display for TargetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// A captured frame, tagged with its scenario step. We keep `raw` rather than
/// the typed payload because the schema layer wants to compare key-sets, and
/// the diff layer wants the original JSON to attribute drift.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    /// Logical step that produced this frame, e.g. `"thread/start"` or
    /// `"turn/start.notifications"`. Used to correlate codex vs bridge frames.
    pub step: String,
    /// Either `"response"` or `"notification"`.
    pub kind: FrameKind,
    /// JSON-RPC method name. For responses this echoes the request method (we
    /// fill it in client-side so the diff layer can group by method without
    /// tracking ids).
    pub method: String,
    /// Raw JSON-RPC frame as it came off the wire.
    pub raw: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameKind {
    Response,
    Notification,
}

#[derive(Debug, Clone)]
pub struct Transcript {
    pub target: TargetId,
    pub frames: Vec<Frame>,
}

impl Transcript {
    pub fn new(target: TargetId) -> Self {
        Self {
            target,
            frames: Vec::new(),
        }
    }

    pub fn push(&mut self, frame: Frame) {
        self.frames.push(frame);
    }

    /// All response frames in order.
    pub fn responses(&self) -> impl Iterator<Item = &Frame> {
        self.frames.iter().filter(|f| f.kind == FrameKind::Response)
    }

    /// All notification frames in order.
    pub fn notifications(&self) -> impl Iterator<Item = &Frame> {
        self.frames
            .iter()
            .filter(|f| f.kind == FrameKind::Notification)
    }

    /// Notification methods, in wire order. The pattern check uses this to
    /// diff sequences across targets.
    pub fn notification_methods(&self) -> Vec<&str> {
        self.notifications().map(|f| f.method.as_str()).collect()
    }
}
