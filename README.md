# Alleycat

![Alleycat logo](assets/alleycat-logo.png)

Iroh-backed bridge that multiplexes a few local coding agents — Codex, Pi, OpenCode, and Claude — onto a single QUIC connection. Run the daemon on your machine, scan a QR with a paired client, and the client picks an agent over the same stream multiplexer.

## Install

Pick the channel you already use. The shipped command is always `alleycat`.

| Platform | Install |
|---|---|
| macOS (Homebrew) | `brew install alleycat/tap/alleycat` |
| macOS / Linux (shell) | `curl -sSf https://github.com/alleycat/alleycat/releases/latest/download/alleycat-installer.sh \| sh` |
| Windows (PowerShell) | `irm https://github.com/alleycat/alleycat/releases/latest/download/alleycat-installer.ps1 \| iex` |
| Windows (MSI) | Download `.msi` from the [latest release](https://github.com/alleycat/alleycat/releases/latest) |
| npm / bun | `npm install -g alley-cat` &nbsp;or&nbsp; `bunx alley-cat` |
| Rust | `cargo install alleycat` |

> The npm package is named `alley-cat` because `alleycat` is taken on the registry; the installed binary is still `alleycat`.

## First run

```bash
alleycat install         # autostart at login (no admin)
alleycat status          # node id, token, agent availability
alleycat pair --qr       # phone-side QR
```

`alleycat install` registers a launchd user agent on macOS, a systemd `--user` unit on Linux (with `.desktop` autostart fallback), or a Startup-folder shortcut on Windows. None of them require sudo.

The daemon spawns external coding-agent CLIs on demand — install whichever ones you'll use:

| Agent | Install |
|---|---|
| `claude` | `npm install -g @anthropic-ai/claude-code` (or `bun install -g @anthropic-ai/claude-code`). Then `claude /login` once. |
| `opencode` | See [opencode docs](https://opencode.ai). |
| `pi-coding-agent` | See pi-mono docs. |
| `codex` | BYO `codex app-server` listening on `127.0.0.1:8390` (configurable). |

## Commands

| Command | What it does |
|---|---|
| `alleycat serve` | Run the daemon in the foreground (what `install` autostarts). |
| `alleycat install` / `uninstall` | Per-user autostart, idempotent. |
| `alleycat status [--json]` | Pid, node id, token fingerprint, uptime, agent availability. Falls back to a file-only readout if the daemon isn't running. |
| `alleycat pair [--qr]` | Print the stable pair payload, optionally with an ASCII QR code. |
| `alleycat rotate` | Mint a fresh token. Node id is preserved; the running daemon picks up the new token immediately. |
| `alleycat reload` | Re-read `host.toml` and swap agent config without restarting. |
| `alleycat agents list` | List configured agents and their availability. |
| `alleycat logs [-f]` | Tail the daemon log files. |
| `alleycat stop` | Graceful shutdown via the control socket. |

The daemon talks to the CLI over a Unix domain socket on macOS/Linux and a per-user named pipe on Windows. `status`, `pair`, and `rotate` round-trip through it when the daemon is up and fall back to file-only operations when it isn't, so first-run flows still work.

## What the daemon spawns

| Agent | Spawned by daemon? | How |
|---|---|---|
| `codex` | No — BYO | Raw TCP proxy to `127.0.0.1:8390` (configurable). You run `codex app-server` yourself. |
| `pi` | Yes, per codex thread | `PiPool` spawns `pi-coding-agent --mode rpc` on demand, bounded at 16 processes with a 10-minute idle reap and LRU eviction. |
| `opencode` | Yes, one shared backend | Lazy spawn of `opencode serve --port=auto --auth-token=auto` on first connect, gated on `/global/health`. Or set `OPENCODE_BRIDGE_BACKEND_URL` to point at an existing instance. |
| `claude` | Yes, per codex thread | `ClaudePool` spawns `claude -p --input-format stream-json --output-format stream-json --session-id <thread_id> --dangerously-skip-permissions` on demand. Same 16-cap, 10-minute idle reap, LRU eviction as pi. Sessions resume on next access via `--resume <thread_id>`. |

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

The daemon answers with `{ok, agents?, error?}`. On `connect`, after the response the stream becomes the agent's native wire — raw codex app-server bytes for `codex`, JSON-RPC over JSONL for `pi`, `opencode`, and `claude`.

## Configuration

`host.toml` is created on first run with sensible defaults; edit and `alleycat reload` to apply.

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

[agents.claude]
enabled = true
bin = "claude"
```

Reload swaps config that's read per-request (token, agent enable flags, codex host/port). Pi's `bin` and OpenCode's `bin`/runtime port are pinned at first construction; changing those still requires `alleycat stop` + `serve`.

## File layout

Per-OS, via `directories::ProjectDirs`:

|              | macOS                                                         | Linux                                | Windows                                   |
|--------------|---------------------------------------------------------------|--------------------------------------|-------------------------------------------|
| Config       | `~/Library/Application Support/dev.Alleycat.alleycat/host.toml` | `$XDG_CONFIG_HOME/alleycat/host.toml` | `%APPDATA%\Alleycat\alleycat\config\host.toml` |
| State        | (collapses to config dir) — `host.key`, `host.lock`, `daemon.pid` | `$XDG_STATE_HOME/alleycat/`        | `%LOCALAPPDATA%\Alleycat\alleycat\data\`  |
| Logs         | `~/Library/Logs/dev.Alleycat.alleycat/daemon.log`             | `$XDG_STATE_HOME/alleycat/logs/`     | `%LOCALAPPDATA%\Alleycat\alleycat\logs\`  |
| Control IPC  | `$TMPDIR/alleycat-<userhash>/control.sock`                    | `$XDG_RUNTIME_DIR/alleycat-<userhash>/control.sock` | `\\.\pipe\alleycat-control-<userhash>` |
| Autostart    | `~/Library/LaunchAgents/dev.alleycat.alleycat.plist`          | `~/.config/systemd/user/alleycat.service` (or `~/.config/autostart/alleycat.desktop`) | `…\Startup\alleycat.lnk` |

The Unix control socket falls through `XDG_RUNTIME_DIR` → state dir → `TMPDIR` → `/tmp` so the path always fits in `sockaddr_un.sun_path` (104 bytes on macOS/BSD, 108 on Linux), even under deeply nested hermetic test homes.

## Notes

- Codex is intentionally BYO: the daemon assumes you already have `codex app-server` listening on `agents.codex.host:port`.
- Pi, OpenCode, and Claude children inherit `kill_on_drop` semantics, so they exit when the daemon does.
- `alleycat stop` shuts the iroh endpoint and the daemon process; launchd / systemd will restart it under their normal supervision.

## Building from source

```bash
cargo install --locked --path crates/alleycat
# or, for a workspace-relative build:
cargo build --release -p alleycat
target/release/alleycat install
```

The workspace has six crates:

- `crates/alleycat` — `alleycat` daemon binary. Owns the iroh endpoint, the persistent identity, the agent dispatcher, and an OS-native control socket so the CLI can talk to the running daemon.
- `crates/bridge-core` — shared JSON-RPC framing, server scaffolding, and notification plumbing used by the bridges.
- `crates/codex-proto` — shared codex `app-server` v2 wire shapes used by every bridge.
- `crates/pi-bridge` — `pi-coding-agent` process pool plus a codex-shaped JSON-RPC translator (one pi process per codex thread).
- `crates/opencode-bridge` — single shared `opencode serve` backend wrapped in the same JSON-RPC surface.
- `crates/claude-bridge` — `claude -p --output-format stream-json` process pool wrapped in the same JSON-RPC surface (one claude process per codex thread).

Releases are produced by [`dist`](https://github.com/axodotdev/cargo-dist) — see `dist-workspace.toml` and `.github/workflows/release.yml`. To cut a release: bump `version` in the root `Cargo.toml`, tag `vX.Y.Z`, push. The workflow builds all six platform tarballs, signs + notarizes the macOS binaries (if `APPLE_*` secrets are set), and publishes the Homebrew formula and npm package.
