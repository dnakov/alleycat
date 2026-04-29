//! Handler modules. Dispatch lives in [`crate::server::dispatch_request`].
//!
//! `lifecycle`, `config`, `mcp`, `model`, `skills`, `command_exec` are owned
//! by claude-stubs (#4). `thread` and `turn` (when added by claude-translate
//! in #3) own the conversation flow.

pub mod command_exec;
pub mod config;
pub mod lifecycle;
pub mod mcp;
pub mod model;
pub mod skills;
pub mod thread;
pub mod turn;
