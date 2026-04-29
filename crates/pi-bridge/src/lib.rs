//! `alleycat-pi-bridge` — codex app-server façade over `pi-coding-agent`.
//!
//! Public surface:
//! - [`PiBridge`] / [`PiBridgeBuilder`] — the unified entry point implementing
//!   `bridge_core::Bridge`.
//! - [`PiSessionRef`] — pi-specific metadata flattened into the on-disk index.

pub mod approval;
pub mod bridge;
pub mod codex_proto;
pub mod handlers;
pub mod index;
pub mod pool;
pub mod state;
pub mod translate;

pub use bridge::{PiBridge, PiBridgeBuilder};
pub use index::{PiSessionRef, ThreadIndex};
