// Self-contained mirror of the codex app-server JSON-RPC wire shapes.
// Hand-written serde types matching codex-rs/app-server-protocol/src/protocol/v2.rs
// and v1.rs (for InitializeParams/InitializeResponse). We mirror only the methods
// listed in the bridge plan; extend as new codex methods get bridged.
//
// Source-of-truth on the codex side:
//   ~/dev/codex/codex-rs/app-server-protocol/src/protocol/v1.rs
//   ~/dev/codex/codex-rs/app-server-protocol/src/protocol/v2.rs
//   ~/dev/codex/codex-rs/app-server-protocol/src/protocol/common.rs (method registration)
//
// Strategy:
// - Wire is camelCase except where noted.
// - Tagged enums use `tag = "type"` exactly as codex emits them.
// - Heavily-nested config / permission / hook / mcp-elicitation shapes that the
//   bridge does not introspect are left as `serde_json::Value`.

pub mod account;
pub mod command_exec;
pub mod common;
pub mod config;
pub mod items;
pub mod jsonrpc;
pub mod lifecycle;
pub mod mcp;
pub mod model;
pub mod notifications;
pub mod skills;
pub mod thread;
pub mod turn;

pub use account::*;
pub use command_exec::*;
pub use common::*;
pub use config::*;
pub use items::*;
pub use jsonrpc::*;
pub use lifecycle::*;
pub use mcp::*;
pub use model::*;
pub use notifications::*;
pub use skills::*;
pub use thread::*;
pub use turn::*;
