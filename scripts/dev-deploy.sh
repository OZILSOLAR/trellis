#!/usr/bin/env bash
# Deploys trellis_core to the local devnet (docker compose `deployer` service)
# and writes frontend/.env.local so the dev server points at it (#502).
set -euo pipefail

RPC_URL="${RPC_URL:-http://stellar:8000/soroban/rpc}"
PASSPHRASE="Standalone Network ; February 2017"
OUT="${ENV_OUT:-/workspace/frontend/.env.local}"

stellar network add local --rpc-url "$RPC_URL" --network-passphrase "$PASSPHRASE" >/dev/null
for _ in $(seq 1 60); do
  curl -s -X POST "$RPC_URL" -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' | grep -q '"status":"healthy"' && break
  sleep 2
done

stellar keys generate deployer --network local --fund >/dev/null 2>&1 || stellar keys fund deployer --network local >/dev/null
cargo rustc --manifest-path contracts/trellis_core/Cargo.toml \
  --crate-type=cdylib --target=wasm32-unknown-unknown --release >/dev/null
CONTRACT_ID=$(stellar contract deploy \
  --wasm target/wasm32-unknown-unknown/release/trellis_core.wasm \
  --source deployer --network local | tail -1)

cat > "$OUT" <<ENV
VITE_CONTRACT_ID=$CONTRACT_ID
VITE_RPC_URL=${PUBLIC_RPC_URL:-http://localhost:8000/soroban/rpc}
VITE_NETWORK_PASSPHRASE=$PASSPHRASE
ENV
echo "Deployed trellis_core: $CONTRACT_ID"
