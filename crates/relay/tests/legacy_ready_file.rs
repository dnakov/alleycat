use std::collections::BTreeSet;

use alleycat::{RelayConfig, RelayRuntime};
use alleycat_protocol::Target;

#[tokio::test]
async fn legacy_bind_produces_unchanged_ready_file_shape() {
    let runtime = RelayRuntime::bind(RelayConfig {
        bind: "127.0.0.1".into(),
        udp_port: 0,
        ready_file: None,
        allowlist: vec![Target::Tcp {
            host: "127.0.0.1".into(),
            port: 1,
        }],
    })
    .await
    .expect("legacy bind");

    let ready = runtime.ready();
    let json = serde_json::to_string(&*ready).expect("serialize ReadyFile");
    let value: serde_json::Value = serde_json::from_str(&json).expect("parse");
    let keys: BTreeSet<&str> = value
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    let expected: BTreeSet<&str> = [
        "protocolVersion",
        "udpPort",
        "certFingerprint",
        "token",
        "pid",
        "allowlist",
    ]
    .into_iter()
    .collect();
    assert_eq!(
        keys, expected,
        "ReadyFile JSON shape changed — existing scripts and the legacy `relay --ready-file` integration depend on the exact key set"
    );
}
