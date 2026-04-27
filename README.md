# Alleycat

![Alleycat logo](assets/alleycat-logo.png)

Iroh-backed bridge that multiplexes a few local coding agents — Codex, Pi, and OpenCode — onto a single QUIC connection. Run the daemon on your machine, scan a QR with a paired client, and the client picks an agent over the same stream multiplexer.

Four crates:

- `crates/alleycat` — `alleycat` daemon binary. Owns the iroh endpoint, the pair payload, and the agent dispatcher.
- `crates/bridge-core` — shared JSON-RPC framing, server scaffolding, and notification plumbing used by the bridges.
- `crates/pi-bridge` — `pi-coding-agent` process pool plus a codex-shaped JSON-RPC translator (one pi process per codex thread).
- `crates/opencode-bridge` — single shared `opencode serve` backend wrapped in the same JSON-RPC surface.

## Quick start

```bash
cargo build --release -p alleycat        # or grab a binary from ./dist
target/release/alleycat serve            # foreground daemon
target/release/alleycat pair --qr        # stable pair payload + ASCII QR for the phone
target/release/alleycat status           # node id, token fingerprint, agent availability
```

The daemon persists its iroh secret and token under `~/.alleycat/`, so the QR you scan once stays valid across restarts. There is no built-in service installer; wire `alleycat serve` into launchd / systemd / Task Scheduler yourself.

| Command | What it does |
|---|---|
| `alleycat serve` | Bind the iroh endpoint and serve agent streams until Ctrl-C. |
| `alleycat pair [--qr]` | Print the stable pair payload as JSON, optionally with a QR code. |
| `alleycat rotate` | Mint a fresh token (node id stays the same). |
| `alleycat status` | Node id, token sha256 fingerprint, config path, per-agent availability. |

## What the daemon spawns

| Agent | Spawned by daemon? | How |
|---|---|---|
| `codex` | No — BYO | Raw TCP proxy to `127.0.0.1:8390` (configurable). You run `codex app-server` yourself. |
| `pi` | Yes, per codex thread | `PiPool` spawns `pi-coding-agent --mode rpc` on demand, bounded at 16 processes with a 10-minute idle reap and LRU eviction. |
| `opencode` | Yes, one shared backend | Lazy spawn of `opencode serve --port=auto --auth-token=auto` on first connect, gated on `/global/health`. |

OpenCode can also point at an existing backend by setting `OPENCODE_BRIDGE_BACKEND_URL` (and optionally `OPENCODE_BRIDGE_AUTH_TOKEN`).

## Pair payload

`alleycat pair` prints:

```json
{
  "v": 1,
  "node_id": "<iroh public key>",
  "token": "<32-byte hex>",
  "relay": null
}
```

`relay` is optional and only set if the operator pinned a specific iroh relay in `host.toml`; otherwise iroh's default discovery applies. There is no port or cert fingerprint — iroh handles transport, the token authenticates the first JSON frame on every stream.

## Wire

ALPN `alleycat/1`. Each iroh bidirectional stream begins with a length-prefixed JSON request:

```json
{"op": "list_agents", "v": 1, "token": "..."}
```
or
```json
{"op": "connect", "v": 1, "token": "...", "agent": "codex"}
```

The daemon answers with `{ok, agents?, error?}`. On `connect`, after the response the stream becomes the agent's native wire — raw codex app-server bytes for `codex`, JSON-RPC over JSONL for `pi` and `opencode`.

## Configuration

`~/.alleycat/host.toml` is created on first run with sensible defaults; edit and restart `alleycat serve` to apply.

```toml
token = "..."          # 32 bytes hex; rotate via `alleycat rotate`
# relay = "https://..." # optional iroh relay override

[agents.codex]
enabled = true
host = "127.0.0.1"
port = 8390

[agents.pi]
enabled = true
bin = "pi-coding-agent"

[agents.opencode]
enabled = true
bin = "opencode"
```

Other state in `~/.alleycat/`:

- `host.key` — 32-byte iroh secret, mode `0600`, generated on first run.
- `host.lock` — fd lock so only one `alleycat serve` runs at a time.

## Notes

- Codex is intentionally BYO: the daemon assumes you already have `codex app-server` listening on `agents.codex.host:port`.
- Pi and OpenCode children inherit `kill_on_drop` semantics, so they exit when the daemon does.
- Unix-socket agent targets are not part of the current surface; everything runs over TCP or stdio.
