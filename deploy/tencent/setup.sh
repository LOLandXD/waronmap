#!/bin/bash
# Run on the Tencent Cloud Ubuntu server.
# Usage: sudo ./deploy/tencent/setup.sh your-email@example.com
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "Please run this script with sudo."
  exit 1
fi

RUN_USER="${SUDO_USER:-ubuntu}"

ensure_swap() {
  local total_mem_kb total_mem_gb swap_total_kb swap_gb
  total_mem_kb=$(awk '/MemTotal/{print $2}' /proc/meminfo)
  total_mem_gb=$((total_mem_kb / 1024 / 1024))
  swap_total_kb=$(awk '/SwapTotal/{print $2}' /proc/meminfo)
  swap_gb=$((swap_total_kb / 1024 / 1024))
  if [[ $total_mem_gb -lt 4 && $swap_gb -lt 4 ]]; then
    echo "==> Low memory (${total_mem_gb}GB RAM, ${swap_gb}GB swap). Creating 4GB swap file for Rust build..."
    if [[ -f /swapfile ]]; then
      swapon /swapfile 2>/dev/null || true
    else
      fallocate -l 4G /swapfile || dd if=/dev/zero of=/swapfile bs=1M count=4096
      chmod 600 /swapfile
      mkswap /swapfile
      swapon /swapfile
      echo '/swapfile none swap sw 0 0' >> /etc/fstab
    fi
    echo "    Swap enabled."
  fi
}
ensure_swap

EMAIL="${1:-}"
DOMAIN="${2:-waronmaps.com}"

if [[ -z "$EMAIL" ]]; then
  echo "WARNING: No email provided. Certbot will register without email."
  echo "Usage: sudo ./deploy/tencent/setup.sh [your-email@example.com] [domain.com]"
fi

ROOT_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
RUNTIME_DIR="$ROOT_DIR/.runtime"
SERVER_PORT=8002
WS_PORT=8003
SERVER_BIN="$ROOT_DIR/rust_server/target/release/node_game_server"
FETCH_BIN="$ROOT_DIR/rust_server/target/release/fetch_region_osm"
GENERATE_BIN="$ROOT_DIR/rust_server/target/release/generate_nodes"
PREPARE_BIN="$ROOT_DIR/rust_server/target/release/prepare_region_cache"
INTERSECTIONS_CSV="$ROOT_DIR/local_node_store/northern_new_england/intersections.csv"

mkdir -p "$RUNTIME_DIR"
chown "$RUN_USER:$RUN_USER" "$RUNTIME_DIR"

echo "==> Installing system packages..."
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y \
  curl git build-essential pkg-config libssl-dev \
  nginx certbot python3-certbot-nginx sudo

if ! command -v rustc >/dev/null 2>&1; then
  echo "==> Installing Rust..."
  sudo -u "$RUN_USER" bash -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y'
fi

# Make cargo available for the rest of the script
export PATH="/home/$RUN_USER/.cargo/bin:$PATH"

echo "==> Building Rust server..."
sudo -u "$RUN_USER" bash -c "cd '$ROOT_DIR' && cargo build --release --manifest-path rust_server/Cargo.toml"

echo "==> Setting up systemd service..."
cp "$ROOT_DIR/deploy/tencent/waronmaps.service" /etc/systemd/system/waronmaps.service
sed -i "s|__ROOT_DIR__|$ROOT_DIR|g" /etc/systemd/system/waronmaps.service
sed -i "s|__USER__|$RUN_USER|g" /etc/systemd/system/waronmaps.service
systemctl daemon-reload
systemctl enable waronmaps

echo "==> Preparing node data (skip if already present)..."
if [[ ! -f "$INTERSECTIONS_CSV" ]]; then
  sudo -u "$RUN_USER" bash -c "cd '$ROOT_DIR' && '$FETCH_BIN' '$ROOT_DIR' >> '$RUNTIME_DIR/builder.log' 2>&1"
  echo "    Fetched OSM road data."
fi
sudo -u "$RUN_USER" bash -c "cd '$ROOT_DIR' && '$GENERATE_BIN' '$ROOT_DIR' >> '$RUNTIME_DIR/builder.log' 2>&1"
sudo -u "$RUN_USER" bash -c "cd '$ROOT_DIR' && '$PREPARE_BIN' '$ROOT_DIR' >> '$RUNTIME_DIR/builder.log' 2>&1"

echo "==> Starting game server..."
systemctl restart waronmaps

echo "==> Configuring Nginx..."
cp "$ROOT_DIR/deploy/tencent/waronmaps.conf" /etc/nginx/sites-available/waronmaps
sed -i "s|__DOMAIN__|$DOMAIN|g" /etc/nginx/sites-available/waronmaps
ln -sf /etc/nginx/sites-available/waronmaps /etc/nginx/sites-enabled/waronmaps
rm -f /etc/nginx/sites-enabled/default
nginx -t
systemctl restart nginx

echo "==> Obtaining SSL certificate for $DOMAIN..."
CERTBOT_ARGS=(--nginx -d "$DOMAIN" --non-interactive --agree-tos --redirect)
if [[ -n "$EMAIL" ]]; then
  CERTBOT_ARGS+=(-m "$EMAIL")
else
  CERTBOT_ARGS+=(--register-unsafely-without-email)
fi
if certbot "${CERTBOT_ARGS[@]}"; then
  echo "    Certificate installed."
else
  echo "    WARNING: certbot failed. Make sure $DOMAIN's A record points to this server's public IP"
  echo "    and ports 80/443 are open. You can retry later with:"
  echo "      sudo certbot --nginx -d $DOMAIN --redirect"
fi

echo ""
echo "Deployment complete."
echo "If the certificate succeeded, open: https://$DOMAIN/openfreemap_viewer.html"
