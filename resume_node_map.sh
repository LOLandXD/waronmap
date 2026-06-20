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

mkdir -p "$RUNTIME_DIR"
cd "$ROOT_DIR"

process_matches() {
  local pid="$1"
  local pattern="$2"
  local cmd
  cmd="$(ps -p "$pid" -o command= 2>/dev/null || true)"
  [[ -n "$cmd" && "$cmd" == *"$pattern"* ]]
}

is_port_open() {
  python3 - "$1" <<'PY'
import socket
import sys

port = int(sys.argv[1])
sock = socket.socket()
sock.settimeout(0.2)
try:
    result = sock.connect_ex(("127.0.0.1", port))
    print("open" if result == 0 else "closed")
finally:
    sock.close()
PY
}

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

build_server() {
  if [[ ! -x "$SERVER_BIN" || "$SERVER_MAIN" -nt "$SERVER_BIN" || "$SERVER_MANIFEST" -nt "$SERVER_BIN" ]]; then
    cargo build --release --manifest-path "$SERVER_MANIFEST"
  fi
}

start_server() {
  if pgrep -f "node_game_server $SERVER_PORT" >/dev/null 2>&1; then
    echo "Game server already running on port $SERVER_PORT"
  else
    build_server
    release_conflicting_port
    if [[ "$(is_port_open "$SERVER_PORT")" == "open" ]]; then
      echo "Port $SERVER_PORT is already in use by another process"
      return 1
    fi
    nohup "$SERVER_BIN" "$SERVER_PORT" >"$SERVER_LOG" 2>&1 &
    echo $! > "$RUNTIME_DIR/game_server.pid"
    echo "Started Rust game server on port $SERVER_PORT"
  fi
}

builder_is_running() {
  local pid_file="$RUNTIME_DIR/builder.pid"
  if [[ -f "$pid_file" ]]; then
    local pid
    pid="$(cat "$pid_file")"
    if kill -0 "$pid" >/dev/null 2>&1 && process_matches "$pid" "prepare_local_region_from_overpass.py"; then
      return 0
    fi
    rm -f "$pid_file"
  fi
  if pgrep -f "python3 prepare_local_region_from_overpass.py --region-id new_england_two" >/dev/null 2>&1; then
    return 0
  fi
  return 1
}

builder_is_complete() {
  python3 <<'PY'
import json
from pathlib import Path

status_path = Path("local_node_store/northern_new_england/build_status.json")
metadata_path = Path("local_node_store/northern_new_england/metadata.json")

if metadata_path.exists():
    print("yes")
elif status_path.exists():
    try:
        status = json.loads(status_path.read_text(encoding="utf-8"))
        print("yes" if status.get("phase") == "complete" else "no")
    except Exception:
        print("no")
else:
    print("no")
PY
}

start_builder() {
  if [[ "$(builder_is_complete)" == "yes" ]]; then
    echo "Builder already complete"
    return
  fi

  if builder_is_running; then
    echo "Builder already running"
    return
  fi

  nohup python3 prepare_local_region_from_overpass.py \
    --region-id new_england_two \
    --batch-step-deg 0.2 \
    --query-timeout-s 35 \
    --retries 3 \
    --min-degree 3 \
    --min-zoom 8 \
    --max-zoom 14 >"$BUILDER_LOG" 2>&1 &
  echo $! > "$RUNTIME_DIR/builder.pid"
  echo "Started builder for Maine and New Hampshire"
}

start_server
start_builder

echo "Open: http://localhost:$SERVER_PORT/openfreemap_viewer.html"
