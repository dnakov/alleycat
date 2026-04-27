# Alleycat

![Alleycat logo](assets/alleycat-logo.png)

Small QUIC tunnel for routable hosts. A phone (or any client) reaches a TCP or Unix-socket target on your machine over a self-signed, fingerprint-pinned, token-authenticated QUIC connection.

Three crates:

- `crates/protocol` — wire types shared by relay and client
- `crates/client` — Rust client library
- `crates/relay` — relay daemon and the `alleycat` binary

## Quick start (service mode)

Long-lived daemon, identity persisted across reboots so the QR you scan once stays valid. Works on macOS, Linux, and Windows; no admin needed.

```bash
cargo install --path crates/relay     # or grab a binary from ./dist
alleycat install                      # per-user autostart (launchd / systemd --user / Startup folder)
alleycat allow add tcp 127.0.0.1:8390
alleycat qr                           # prints QR + JSON for the phone
alleycat status
```

| Command | What it does |
|---|---|
| `alleycat run` | Foreground daemon (what `install` autostarts) |
| `alleycat install` / `uninstall` | Per-user autostart, idempotent |
| `alleycat status [--json]` | Up? Port, fingerprint, allowlist, host candidates |
| `alleycat qr [--image PATH]` | QR + JSON for the phone |
| `alleycat rotate` | Mint fresh cert + token (drains existing sessions over 5 s) |
| `alleycat allow add\|rm tcp HOST:PORT` / `unix PATH`, `alleycat allow list` | Live-edit allowlist |
| `alleycat logs [-f]` | Tail the daemon log |
| `alleycat stop` / `reload` | Exit cleanly / re-read `config.toml` |

Config, state, and logs live under conventional per-OS paths (`~/Library/Application Support/...` on macOS, XDG dirs on Linux, `%APPDATA%`/`%LOCALAPPDATA%` on Windows). Edit the allowlist, host overrides, and log level via `alleycat reload` after editing the TOML.

## One-shot mode (no daemon)

For scripts and demos. Ephemeral identity per launch:

```bash
# pair: prints a QR, runs until Ctrl-C
alleycat pair --allow-tcp 127.0.0.1:8390

# relay: writes a ready file, runs until Ctrl-C
alleycat relay --bind 0.0.0.0 --udp-port 0 \
  --ready-file /tmp/alleycat-ready.json \
  --allow-tcp 127.0.0.1:8390
```

Ready file shape (used by both legacy modes):

```json
{
  "protocolVersion": 1,
  "udpPort": 43127,
  "certFingerprint": "sha256-hex-without-prefix",
  "token": "random-hex-token",
  "pid": 12345,
  "allowlist": [{ "kind": "tcp", "host": "127.0.0.1", "port": 8390 }]
}
```

## Use the Rust client

```toml
[dependencies]
alleycat-client = { path = "crates/client" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

```rust
use alleycat_client::{ConnectParams, ForwardSpec, Session, Target, WIRE_PROTOCOL_VERSION};

let session = Session::connect(ConnectParams {
    host: "example-host".into(),
    port: 43127,
    cert_fingerprint: "sha256-hex-without-prefix".into(),
    token: "random-hex-token".into(),
    protocol_version: WIRE_PROTOCOL_VERSION,
}).await?;

let forward = session.ensure_forward(ForwardSpec {
    local_port: 0,
    target: Target::Tcp { host: "127.0.0.1".into(), port: 8390 },
}).await?;

println!("local port: {}", forward.local_port());
```

Raw stream instead of a local forward:

```rust
let stream = session.open_stream(Target::Unix {
    path: "/tmp/codex-ipc/ipc-123.sock".into(),
}).await?;
```

Shutdown: `forward.close().await; session.shutdown().await;`

## Notes

- Allowlist matches are exact strings — `127.0.0.1:8390` does not match `localhost:8390`.
- Unix-socket targets work on macOS and Linux only.
- Intentionally narrow: no broker, no NAT traversal, no reverse mode, no UDP proxying.
