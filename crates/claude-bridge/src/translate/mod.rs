//! Wire-format translation between claude `stream-json` events and codex
//! JSON-RPC notifications + items.
//!
//! Three layers, mirroring `crates/pi-bridge/src/translate/`:
//!
//! - [`tool_call`] — classify a claude tool name into a codex `ThreadItem` kind
//!   (`Bash → CommandExecution`, `Edit|Write|MultiEdit|NotebookEdit →
//!   FileChange`, `mcp__<server>__<tool> → Mcp`, everything else → `Dynamic`).
//! - [`input`] — `Vec<UserInput>` → claude stream-json user message envelope.
//! - [`events`] — the live-event translator. Per-turn `EventTranslatorState`
//!   carries (thread_id, turn_id) plus per-content-block-index open-item
//!   bookkeeping plus per-tool_use_id open-tool-call bookkeeping.
//! - [`items`] — read on-disk claude transcript JSONL (under
//!   `~/.claude/projects/<encoded-cwd>/<session_id>.jsonl`) → `Vec<Turn>` of
//!   `Vec<ThreadItem>` for `thread/read` / `thread/resume` hydration.

pub mod events;
pub mod input;
pub mod items;
pub mod tool_call;
