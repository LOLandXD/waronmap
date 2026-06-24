# waronmap

Persistent multiplayer road-based node strategy game built with Rust and MapLibre.

## What Runs

- Runtime server: Rust in `rust_server/`
- Viewer: `openfreemap_viewer.html`
- S2/Hilbert node generator: Rust binary in `rust_server/src/bin/generate_nodes.rs`
- Cache/prepare step: Rust binary in `rust_server/src/bin/prepare_region_cache.rs`

The project runtime is Rust-only.

## Rendering And UI

- HTML is only used for 2D overlay UI:
  - login / top status panel
  - node info panel
  - small control buttons
- Nodes are rendered with MapLibre GL, which uses WebGL.
- Connection roads and transport arrows are rendered on a canvas overlay, not as HTML elements.
- The goal is to keep gameplay rendering out of the DOM.

## Networking

- Backend is written in Rust in `rust_server/`.
- Data exchange uses JSON.
- HTTP JSON endpoints remain available for auth, bootstrap, and request-style actions.
- Live world updates are pushed over a WebSocket on `ws://localhost:8003/ws` so connected clients see changes immediately instead of polling.

## Current Gameplay Flow

1. Register or log in.
2. Click one of your own green nodes.
3. Use the bottom-right node info panel.
4. In `Adjacent Targets`, click `Connect` on a directly adjacent node.
5. Adjust `army per tick` from the same panel.

## Project Structure

- `openfreemap_viewer.html`: main frontend shell, overlays, canvas connection rendering
- `rust_server/src/main.rs`: Rust backend, auth, world state, S2 spatial index, WebSocket broadcaster, JSON APIs
- `rust_server/src/bin/generate_nodes.rs`: Rust generator that sorts intersections by S2 Hilbert curve
- `rust_server/src/bin/prepare_region_cache.rs`: Rust cache/prepare step for prepared region data
- `local_node_store/`: local region data and state boundaries
- `vendor/`: local frontend dependencies
- `sw.js`: service worker cache versioning

## Dependencies

Rust deps are installed automatically by Cargo when you build or run:

```bash
cargo build --manifest-path rust_server/Cargo.toml
```

- No Python dependency is required for runtime.

## Development

Run the app with:

```bash
./resume_node_map.sh
```

This now does 3 Rust-only steps:

- generate S2/Hilbert-sorted node data
- refresh cached region status files
- start the Rust game server

The HTTP server listens on port `8002` and the WebSocket server listens on port `8003`. Set `WS_PORT` to change the WebSocket port.

Open:

```text
http://localhost:8002/openfreemap_viewer.html
```

## .gitignore

Current root `.gitignore` entries:

```gitignore
.DS_Store
*.pyc
__pycache__/
.runtime/
local_node_store/northern_new_england/
rust_server/target/
state_pbf/
game_data/state.json
```

## License

MIT
