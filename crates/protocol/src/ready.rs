use serde::{Deserialize, Serialize};

use crate::Target;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadyFile {
    pub protocol_version: u32,
    pub udp_port: u16,
    pub cert_fingerprint: String,
    pub token: String,
    pub pid: u32,
    pub allowlist: Vec<Target>,
}
