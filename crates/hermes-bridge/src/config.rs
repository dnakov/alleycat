//! Hermes bridge configuration.

use serde::{Deserialize, Serialize};

use crate::api_client::DEFAULT_API_BASE;

/// Connection mode for the Hermes Agent backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum HermesMode {
    /// Connect to an already-running Hermes gateway over HTTP/SSE.
    Api {
        /// Base URL of the Hermes Agent gateway (e.g. <http://127.0.0.1:8642>).
        api_base: String,
    },
    /// Spawn `hermes` CLI as a subprocess; communicate over stdio.
    Cli {
        /// Path to the `hermes` binary; defaults to "hermes" in PATH.
        bin: Option<String>,
    },
    /// Try API first; fall back to CLI if the gateway is unreachable.
    Auto {
        api_base: String,
        bin: Option<String>,
    },
}

impl Default for HermesMode {
    fn default() -> Self {
        HermesMode::Auto {
            api_base: DEFAULT_API_BASE.to_string(),
            bin: None,
        }
    }
}

/// Top-level configuration for the Hermes bridge.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HermesBridgeConfig {
    #[serde(default)]
    pub mode: HermesMode,
    /// Directory for persistent state (thread index, etc.).
    #[serde(default)]
    pub state_dir: Option<String>,
}
