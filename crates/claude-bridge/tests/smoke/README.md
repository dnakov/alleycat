# claude-bridge smoke drivers

Operator-facing smoke scripts. Not part of `cargo test` — driven by hand
when you want to exercise the bridge against the real `claude` CLI (or a
running `alleycat` daemon).

| Script | What it does |
|---|---|
| `stdio_smoke.sh` | Spawns `cargo run -p alleycat-claude-bridge` in stdio mode and feeds it a scripted JSON-RPC sequence (`initialize` → `thread/start` → `turn/start "say hi in one word"` → wait for `turn/completed`). Talks to **real claude**. Requires `claude` on `$PATH` and `jq`. |
| `daemon_smoke.sh` | Connects to a running `alleycat` daemon via the iroh client (`alleycat probe`) and runs the same flow against the `claude` agent. Requires `alleycat serve` running in another terminal. |

## Running

```bash
# stdio smoke against real claude:
./crates/claude-bridge/tests/smoke/stdio_smoke.sh

# daemon-mediated:
target/release/alleycat serve     # in another terminal
./crates/claude-bridge/tests/smoke/daemon_smoke.sh
```

## CI coverage

The Rust integration tests (`tests/smoke_in_process.rs`,
`tests/smoke_binary.rs`, `tests/fake_claude_smoke.rs`) cover the same
flow via the `fake-claude` test binary, so CI doesn't depend on a real
`claude` install. These bash drivers are the live-claude / live-daemon
counterparts you run before tagging a release.
