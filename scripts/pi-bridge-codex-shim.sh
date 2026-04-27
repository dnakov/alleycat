#!/usr/bin/env bash
# Wrapper that lets codex's app-server-test-client treat alleycat-pi-bridge as
# if it were the `codex` CLI. The test client invokes `<bin> app-server [...]`;
# we drop the `app-server` token and exec our bridge with the rest.
set -euo pipefail

BRIDGE_BIN="${BRIDGE_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/alleycat-pi-bridge}"

ARGS=()
for arg in "$@"; do
  if [[ "$arg" == "app-server" ]]; then
    continue
  fi
  ARGS+=("$arg")
done

if [[ ${#ARGS[@]} -eq 0 ]]; then
  exec "$BRIDGE_BIN"
else
  exec "$BRIDGE_BIN" "${ARGS[@]}"
fi
