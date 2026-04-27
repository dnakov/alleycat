use alleycat_protocol::Target;
use anyhow::Context;
use qrcodegen::{QrCode, QrCodeEcc};
use serde::{Deserialize, Serialize};

use crate::host_detect;
use crate::{RelayConfig, RelayRuntime};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConnectParams {
    pub protocol_version: u32,
    pub udp_port: u16,
    pub cert_fingerprint: String,
    pub token: String,
    /// Ranked list of hostnames/IPs the phone can try, most-likely-reachable
    /// first. The mobile client races them and uses the first that connects.
    /// Empty array would mean "no auto-host" — `pair` always populates at
    /// least one.
    pub host_candidates: Vec<String>,
}

pub struct PairOptions {
    pub bind: String,
    pub udp_port: u16,
    pub allow_tcp: Vec<Target>,
    pub allow_unix: Vec<String>,
    /// User-supplied host overrides (`--host` flag, repeatable). When non-empty
    /// these REPLACE auto-detection so the user can steer the phone toward a
    /// specific name/IP.
    pub host_overrides: Vec<String>,
}

pub async fn run_pair(opts: PairOptions) -> anyhow::Result<()> {
    let mut allowlist = opts.allow_tcp;
    allowlist.extend(
        opts.allow_unix
            .into_iter()
            .map(|path| Target::Unix { path }),
    );
    if allowlist.is_empty() {
        anyhow::bail!("at least one --allow-tcp or --allow-unix target is required");
    }

    let relay = RelayRuntime::bind(RelayConfig {
        bind: opts.bind,
        udp_port: opts.udp_port,
        ready_file: None,
        allowlist,
    })
    .await?;

    let host_candidates = if opts.host_overrides.is_empty() {
        host_detect::detect_candidates()
    } else {
        opts.host_overrides
    };

    let ready = relay.ready();
    let params = ConnectParams {
        protocol_version: ready.protocol_version,
        udp_port: ready.udp_port,
        cert_fingerprint: ready.cert_fingerprint.clone(),
        token: ready.token.clone(),
        host_candidates: host_candidates.clone(),
    };
    let json = serde_json::to_string(&params).context("serialize connect params")?;

    let qr = QrCode::encode_text(&json, QrCodeEcc::Medium)
        .context("encode connect params as qr code")?;
    print_qr(&qr);
    println!("{json}");
    eprintln!();
    eprintln!("alleycat pair: relay host candidates (override with --host):");
    for (i, h) in host_candidates.iter().enumerate() {
        eprintln!("  {}. {h}", i + 1);
    }

    relay.serve().await
}

pub fn print_qr(qr: &QrCode) {
    let size = qr.size();
    let border: i32 = 2;
    for y in -border..size + border {
        let mut line = String::new();
        for x in -border..size + border {
            if qr.get_module(x, y) {
                line.push_str("\u{2588}\u{2588}");
            } else {
                line.push_str("  ");
            }
        }
        println!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_params_round_trip_through_qr_payload() {
        let params = ConnectParams {
            protocol_version: 1,
            udp_port: 51820,
            cert_fingerprint: "a".repeat(64),
            token: "b".repeat(32),
            host_candidates: vec!["studio.tail.ts.net".into(), "192.168.1.5".into()],
        };
        let json = serde_json::to_string(&params).expect("serialize");
        let qr = QrCode::encode_text(&json, QrCodeEcc::Medium).expect("encode qr");
        assert!(qr.size() > 0);
        let parsed: ConnectParams = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed, params);
    }

    #[test]
    fn connect_params_uses_camel_case() {
        let params = ConnectParams {
            protocol_version: 7,
            udp_port: 1234,
            cert_fingerprint: "ff".repeat(32),
            token: "00".repeat(16),
            host_candidates: vec!["x".into()],
        };
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&params).unwrap()).unwrap();
        assert!(value.get("protocolVersion").is_some());
        assert!(value.get("udpPort").is_some());
        assert!(value.get("certFingerprint").is_some());
        assert!(value.get("token").is_some());
        assert!(value.get("hostCandidates").is_some());
        assert!(value.get("pid").is_none());
        assert!(value.get("allowlist").is_none());
    }
}
