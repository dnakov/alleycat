use std::collections::BTreeSet;

use alleycat::pair::ConnectParams;

#[test]
fn connect_params_json_shape_is_frozen() {
    let params = ConnectParams {
        protocol_version: 1,
        udp_port: 51820,
        cert_fingerprint: "a".repeat(64),
        token: "b".repeat(32),
        host_candidates: vec!["studio.tail.ts.net".into(), "192.168.1.5".into()],
    };
    let json = serde_json::to_string(&params).expect("serialize");
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
        "hostCandidates",
    ]
    .into_iter()
    .collect();
    assert_eq!(
        keys, expected,
        "ConnectParams JSON shape changed — iOS app and Litter integration depend on the exact key set"
    );
}
