#!/usr/bin/env bash
# Runs Firebreak's local Flame devnet in the foreground: the genesis written by
# `firebreak-owner init` into <data>/devnet/chainparams.toml, zero fees, and a short block interval.
#
# This is a LOCAL DEVNET. The block interval is a local demo setting; it says nothing about the
# consensus or finality of any real Flame network.
#
# Usage: scripts/devnet.sh [DATA_DIR]   (default: ./.firebreak)
# Environment: FIREBREAK_RPC_BIND (default 127.0.0.1:7740), FIREBREAK_BLOCK_SECS (default 2).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
data="$(realpath -m "${1:-$root/.firebreak}")"
rpc_bind="${FIREBREAK_RPC_BIND:-127.0.0.1:7740}"
block_secs="${FIREBREAK_BLOCK_SECS:-2}"
flamed="$root/flame-lib/target/release/flamed"
dir="$data/devnet"

if [ ! -f "$dir/chainparams.toml" ]; then
    echo "No $dir/chainparams.toml: run \`firebreak-owner --data-dir $data init\` first." >&2
    exit 1
fi
if [ ! -x "$flamed" ]; then
    cargo build --release --locked --manifest-path "$root/flame-lib/Cargo.toml" \
        -p flamed --features devnet
fi
if [ ! -f "$dir/genesis.json" ]; then
    "$flamed" genesis --chainparams "$dir/chainparams.toml" --out "$dir/genesis.json"
fi

cat > "$dir/flamed.toml" <<TOML
data_dir = "$dir/node"
genesis = "$dir/genesis.json"
rpc_bind = "$rpc_bind"
block_interval_secs = $block_secs
minimum_fee = 0
TOML

exec "$flamed" run --config "$dir/flamed.toml"
