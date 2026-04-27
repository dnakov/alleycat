//! Control protocol types exchanged over the IPC stream between the CLI and
//! the daemon. One request per connection, one response, then close. The
//! wire frame is a length-prefixed JSON envelope provided by
//! `alleycat_protocol::frame::{read_frame_json, write_frame_json}`.

use std::path::PathBuf;

use alleycat_protocol::Target;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Status,
    Qr {
        #[serde(default)]
        image: Option<PathBuf>,
    },
    Rotate,
    AllowAdd {
        target: Target,
    },
    AllowRm {
        target: Target,
    },
    AllowList,
    Reload,
    Stop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl Response {
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            data: None,
        }
    }

    pub fn ok_with<T: Serialize>(data: &T) -> anyhow::Result<Self> {
        Ok(Self {
            ok: true,
            error: None,
            data: Some(serde_json::to_value(data)?),
        })
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
            data: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusInfo {
    pub pid: u32,
    pub udp_port: u16,
    /// First 16 hex chars of the SHA-256 cert fingerprint. Stable across
    /// daemon restarts unless the user invokes `rotate`.
    pub fingerprint_short: String,
    pub allowlist: Vec<Target>,
    pub host_candidates: Vec<String>,
    pub uptime_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RotateResult {
    pub fingerprint_short: String,
}

/// Helper for the daemon side: lowercase hex SHA-256 of the cert DER, then
/// truncated to the first 16 chars. Mirrors [`StatusInfo::fingerprint_short`].
pub fn short_fingerprint(full_hex: &str) -> String {
    full_hex.chars().take(16).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip() {
        let r = Request::Status;
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains(r#""op":"status""#));
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Request::Status));
    }

    #[test]
    fn allow_add_round_trip() {
        let r = Request::AllowAdd {
            target: Target::Tcp {
                host: "127.0.0.1".into(),
                port: 8390,
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        match back {
            Request::AllowAdd {
                target: Target::Tcp { host, port },
            } => {
                assert_eq!(host, "127.0.0.1");
                assert_eq!(port, 8390);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn response_ok_skips_error_and_data() {
        let r = Response::ok();
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, r#"{"ok":true}"#);
    }

    #[test]
    fn response_err_includes_error() {
        let r = Response::err("boom");
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains(r#""ok":false"#));
        assert!(s.contains(r#""error":"boom""#));
    }

    #[test]
    fn short_fingerprint_truncates_to_16() {
        let s = short_fingerprint(&"abcdef0123456789deadbeef".repeat(4));
        assert_eq!(s.len(), 16);
        assert_eq!(s, "abcdef0123456789");
    }
}
