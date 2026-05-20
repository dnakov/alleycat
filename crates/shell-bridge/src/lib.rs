//! `alleycat-shell-bridge` — JSON-RPC shell sessions over PTY-backed child
//! processes.

pub mod bridge;
pub mod session;

pub use bridge::{ShellBridge, ShellBridgeBuilder};
