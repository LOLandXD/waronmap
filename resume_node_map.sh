#!/bin/zsh
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")" && pwd)"
RUNTIME_DIR="$ROOT_DIR/.runtime"
SERVER_LOG="$RUNTIME_DIR/game_server.log"
BUILDER_LOG="$RUNTIME_DIR/builder.log"
SERVER_PORT="${1:-8002}"
SERVER_MANIFEST="$ROOT_DIR/rust_server/Cargo.toml"
SERVER_MAIN="$ROOT_DIR/rust_server/src/main.rs"
SERVER_BIN="$ROOT_DIR/rust_server/target/release/node_game_server"
PREPARE_BIN="$ROOT_DIR/rust_server/target/release/prepare_region_cache"
GENERATE_BIN="$ROOT_DIR/rust_server/target/release/generate_nodes"
GENERATE_MAIN="$ROOT_DIR/rust_server/src/bin/generate_nodes.rs"
PREPARE_MAIN="$ROOT_DIR/rust_server/src/bin/prepare_region_cache.rs"

mkdir -p "$RUNTIME_DIR"
cd "$ROOT_DIR"

release_conflicting_port() {
  local pid
  pid="$(lsof -tiTCP:"$SERVER_PORT" -sTCP:LISTEN 2>/dev/null | head -n 1 || true)"
  if [[ -z "$pid" ]]; then
    return 0
  fi

  local cmd
  cmd="$(ps -p "$pid" -o command= 2>/dev/null || true)"
  if [[ "$cmd" == *"game_server.js"* || "$cmd" == *"node_game_server"* || "$cmd" == *"http.server"* ]]; then
    kill "$pid" >/dev/null 2>&1 || true
    sleep 1
  fi
}

build_binaries() {
  if [[ ! -x "$SERVER_BIN" || ! -x "$PREPARE_BIN" || ! -x "$GENERATE_BIN" || "$SERVER_MAIN" -nt "$SERVER_BIN" || "$PREPARE_MAIN" -nt "$PREPARE_BIN" || "$GENERATE_MAIN" -nt "$GENERATE_BIN" || "$SERVER_MANIFEST" -nt "$SERVER_BIN" || "$SERVER_MANIFEST" -nt "$PREPARE_BIN" || "$SERVER_MANIFEST" -nt "$GENERATE_BIN" ]]; then
    cargo build --release --manifest-path "$SERVER_MANIFEST"
  fi
}

start_server() {
  if pgrep -f "node_game_server $SERVER_PORT" >/dev/null 2>&1; then
    echo "Game server already running on port $SERVER_PORT"
  else
    build_binaries
    release_conflicting_port
    if lsof -tiTCP:"$SERVER_PORT" -sTCP:LISTEN >/dev/null 2>&1; then
      echo "Port $SERVER_PORT is already in use by another process"
      return 1
    fi
    nohup "$SERVER_BIN" "$SERVER_PORT" >"$SERVER_LOG" 2>&1 &
    echo $! > "$RUNTIME_DIR/game_server.pid"
    echo "Started Rust game server on port $SERVER_PORT"
  fi
}

generate_nodes() {
  build_binaries
  "$GENERATE_BIN" "$ROOT_DIR" >"$BUILDER_LOG" 2>&1
  echo "Generated S2/Hilbert-sorted nodes"
}

prepare_region_cache() {
  build_binaries
  "$PREPARE_BIN" "$ROOT_DIR" >>"$BUILDER_LOG" 2>&1
  echo "Prepared cached region files"
}

generate_nodes
prepare_region_cache
start_server

echo "Open: http://localhost:$SERVER_PORT/openfreemap_viewer.html"
