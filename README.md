# Alleycat

![Alleycat logo](assets/alleycat-logo.png)

Alleycat is a small QUIC tunnel for routable hosts.

It has three crates:

- `crates/protocol`: shared wire types
- `crates/client`: Rust client library
- `crates/relay`: relay library plus the `alleycat` binary

## What It Does

- Opens a long-lived QUIC connection to a remote relay
- Authenticates with a per-launch token
- Pins the relay certificate by SHA-256 fingerprint
- Opens per-target streams to exact allowlisted TCP or Unix-socket destinations
- Optionally exposes a local `127.0.0.1:<port>` forward for legacy consumers

## Run The Relay

```bash
cargo run -p alleycat -- relay \
  --bind 0.0.0.0 \
  --udp-port 0 \
  --ready-file /tmp/alleycat-ready.json \
  --allow-tcp 127.0.0.1:8390 \
  --allow-unix /tmp/codex-ipc/ipc-123.sock
```

The relay writes a ready file like:

```json
{
  "protocolVersion": 1,
  "udpPort": 43127,
  "certFingerprint": "sha256-hex-without-prefix",
  "token": "random-hex-token",
  "pid": 12345,
  "allowlist": [
    { "kind": "tcp", "host": "127.0.0.1", "port": 8390 }
  ]
}
```

Use `udpPort`, `certFingerprint`, `token`, and `protocolVersion` to connect.

## Use The Client

Add the client crate:

```toml
[dependencies]
alleycat-client = { path = "crates/client" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Connect and create a local forward:

```rust
use alleycat_client::{ConnectParams, ForwardSpec, Session, Target, WIRE_PROTOCOL_VERSION};

let session = Session::connect(ConnectParams {
    host: "example-host".into(),
    port: 43127,
    cert_fingerprint: "sha256-hex-without-prefix".into(),
    token: "random-hex-token".into(),
    protocol_version: WIRE_PROTOCOL_VERSION,
}).await?;

let forward = session
    .ensure_forward(ForwardSpec {
        local_port: 0,
        target: Target::Tcp {
            host: "127.0.0.1".into(),
            port: 8390,
        },
    })
    .await?;

println!("local port: {}", forward.local_port());
```

Open a raw stream instead:

```rust
let stream = session
    .open_stream(Target::Unix {
        path: "/tmp/codex-ipc/ipc-123.sock".into(),
    })
    .await?;
```

Shutdown:

```rust
forward.close().await;
session.shutdown().await;
```

## Notes

- Allowlist checks are exact. If the relay was launched with `127.0.0.1:8390`, `localhost:8390` is rejected.
- Unix targets only work on platforms that support Unix sockets.
- The relay is intentionally narrow: no broker, NAT traversal, reverse mode, or UDP proxying.
