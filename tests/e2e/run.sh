#!/usr/bin/env bash
# Full user journey E2E test (#143).
#
# Deploys the trellis_core contract to a local Soroban devnet, starts the
# real frontend dev server against it, and drives an actual browser
# (Playwright + Chromium) through creating an agreement via the UI with a
# mocked Freighter wallet, then verifies it reads back correctly on the
# status page.
#
# Requires: docker, the `stellar` CLI (26.x+) on PATH, a
# wasm32-unknown-unknown Rust target, and Node/npm. See README.md in this
# directory.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
FRONTEND_DIR="$REPO_ROOT/frontend"
WASM_PATH="$REPO_ROOT/target/wasm32-unknown-unknown/release/trellis_core.wasm"
RPC_URL="http://localhost:8000/soroban/rpc"
PASSPHRASE="Standalone Network ; February 2017"
DEV_SERVER_PORT=5173

log() { echo "[e2e] $*"; }

DEV_SERVER_PID=""
ENV_BACKUP=""

cleanup() {
  if [ -n "$DEV_SERVER_PID" ] && kill -0 "$DEV_SERVER_PID" 2>/dev/null; then
    log "stopping frontend dev server (pid $DEV_SERVER_PID)"
    kill "$DEV_SERVER_PID" 2>/dev/null || true
    wait "$DEV_SERVER_PID" 2>/dev/null || true
  fi
  if [ -n "$ENV_BACKUP" ]; then
    mv "$ENV_BACKUP" "$FRONTEND_DIR/.env"
    log "restored original frontend/.env"
  elif [ -f "$FRONTEND_DIR/.env.e2e-generated" ]; then
    rm -f "$FRONTEND_DIR/.env"
  fi
  rm -f "$FRONTEND_DIR/.env.e2e-generated"
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Devnet + contract, identical setup to tests/cli_integration/run.sh
# ---------------------------------------------------------------------------

wait_for_healthy() {
  for _ in $(seq 1 30); do
    resp=$(curl -s -X POST "$RPC_URL" -H "Content-Type: application/json" \
      -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' || true)
    if echo "$resp" | grep -q '"status":"healthy"'; then
      return 0
    fi
    sleep 2
  done
  log "local network never became healthy"
  return 1
}

if ! curl -s -X POST "$RPC_URL" -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' 2>/dev/null | grep -q '"status":"healthy"'; then
  log "starting local Soroban devnet container..."
  stellar container start local
  wait_for_healthy
else
  log "local Soroban devnet already running"
fi

for identity in payer payee resolver; do
  if ! stellar keys address "$identity" >/dev/null 2>&1; then
    stellar keys generate "$identity" --network local --fund >/dev/null
  else
    stellar keys fund "$identity" --network local >/dev/null 2>&1 || true
  fi
done

PAYER=$(stellar keys address payer)
PAYEE=$(stellar keys address payee)
RESOLVER=$(stellar keys address resolver)
PAYER_SECRET=$(stellar keys show payer)
PAYEE_SECRET=$(stellar keys show payee)

log "building trellis_core contract (wasm32)..."
cargo rustc --manifest-path "$REPO_ROOT/contracts/trellis_core/Cargo.toml" \
  --crate-type=cdylib --target=wasm32-unknown-unknown --release >/dev/null

log "resolving test token (native XLM SAC)..."
TOKEN=$(stellar contract id asset --asset native --network local 2>/dev/null | tail -1)
if ! stellar contract info interface --contract-id "$TOKEN" --network local >/dev/null 2>&1; then
  stellar contract asset deploy --asset native --source payer --network local >/dev/null 2>&1
fi

log "deploying trellis_core contract..."
CONTRACT_ID=$(stellar contract deploy --wasm "$WASM_PATH" --source payer --network local 2>/dev/null | tail -1)
log "token=$TOKEN contract=$CONTRACT_ID"

# ---------------------------------------------------------------------------
# Frontend dev server, pointed at the local devnet
# ---------------------------------------------------------------------------

if [ -f "$FRONTEND_DIR/.env" ]; then
  ENV_BACKUP=$(mktemp)
  mv "$FRONTEND_DIR/.env" "$ENV_BACKUP"
fi

cat > "$FRONTEND_DIR/.env" <<EOF
VITE_CONTRACT_ID=$CONTRACT_ID
VITE_RPC_URL=$RPC_URL
VITE_NETWORK_PASSPHRASE=$PASSPHRASE
EOF
touch "$FRONTEND_DIR/.env.e2e-generated"

export CI=1

log "installing frontend dependencies..."
timeout 300 npm install --prefix "$FRONTEND_DIR" </dev/null >/dev/null

log "checking for Playwright's Chromium browser..."
if ! compgen -G "${PLAYWRIGHT_BROWSERS_PATH:-$HOME/.cache/ms-playwright}/chromium-*" >/dev/null; then
  log "installing Chromium (browser only — system deps assumed present)..."
  timeout 300 npx --prefix "$FRONTEND_DIR" playwright install chromium </dev/null >/dev/null
fi

log "starting frontend dev server on :$DEV_SERVER_PORT..."
(cd "$FRONTEND_DIR" && nohup npm run dev -- --port "$DEV_SERVER_PORT" --strictPort </dev/null >/tmp/trellis-e2e-vite.log 2>&1 &)
sleep 1
DEV_SERVER_PID=$(pgrep -f "vite.*--port $DEV_SERVER_PORT" | head -1)

log "waiting for dev server to respond..."
for i in $(seq 1 30); do
  if curl -s --max-time 2 -o /dev/null "http://localhost:$DEV_SERVER_PORT/"; then
    break
  fi
  if [ "$i" -eq 30 ]; then
    log "dev server never responded on :$DEV_SERVER_PORT — see /tmp/trellis-e2e-vite.log"
    exit 1
  fi
  sleep 1
done

# ---------------------------------------------------------------------------
# Run the Playwright suite against it
# ---------------------------------------------------------------------------

log "running Playwright E2E suite..."
E2E_BASE_URL="http://localhost:$DEV_SERVER_PORT" \
E2E_PAYER_ADDRESS="$PAYER" \
E2E_PAYER_SECRET="$PAYER_SECRET" \
E2E_PAYEE_ADDRESS="$PAYEE" \
E2E_PAYEE_SECRET="$PAYEE_SECRET" \
E2E_RESOLVER_ADDRESS="$RESOLVER" \
E2E_TOKEN_ADDRESS="$TOKEN" \
E2E_RPC_URL="$RPC_URL" \
E2E_NETWORK_PASSPHRASE="$PASSPHRASE" \
  timeout 180 npm run test:e2e --prefix "$FRONTEND_DIR"
