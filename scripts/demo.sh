#!/usr/bin/env bash
# Runs Firebreak's demonstration from a fresh state on a fresh local devnet.
#
# It wipes the data directory, creates the three parties, starts the node, the app's API and the
# dashboard in the background, and walks through the story: fund an allowance, attack it with the
# delegated key, pay, inspect and spend as the merchant, recover the rest, and attack a recovered
# voucher. Every step is a real command against the real node; nothing is simulated.
#
# Usage: scripts/demo.sh [--auto]
#   --auto   run without pausing, and check the final balances (owner 940, merchant 60,
#            allowance 0); exits non-zero if anything differs.
# Environment: FIREBREAK_DATA (default ./.firebreak). The node listens on 127.0.0.1:7740, the
# app's API on 127.0.0.1:7741 and the dashboard on http://127.0.0.1:7742. Needs curl and jq.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
data="$(realpath -m "${FIREBREAK_DATA:-$root/.firebreak}")"
auto=0
if [ "${1:-}" = "--auto" ]; then
    auto=1
fi
rpc="http://127.0.0.1:7740"
agent_api="127.0.0.1:7741"
dashboard="127.0.0.1:7742"
bin="$root/target/release"

step() {
    printf '\n\033[1;34m== %s\033[0m\n' "$*"
    if [ "$auto" = 0 ]; then
        read -r -p "   (Enter to run) " _
    fi
}

run() {
    printf '\033[2m$ %s\033[0m\n' "$*"
    "$@"
}

# Like `run`, but the command is expected to fail: its failure is the point of the step.
refused() {
    printf '\033[2m$ %s\033[0m\n' "$*"
    if "$@"; then
        echo "UNEXPECTED: that was supposed to be refused." >&2
        exit 1
    fi
}

owner() { "$bin/firebreak-owner" --data-dir "$data" --rpc "$rpc" "$@"; }
agent() { "$bin/firebreak-agent" --data-dir "$data" --rpc "$rpc" "$@"; }
merchant() { "$bin/firebreak-merchant" --data-dir "$data" --rpc "$rpc" "$@"; }
attack() { "$bin/firebreak-attack" --data-dir "$data" --rpc "$rpc" "$@"; }

wait_for() {
    local what="$1" url="$2"
    for _ in $(seq 1 100); do
        if curl -s -m 1 -o /dev/null "$url"; then
            return 0
        fi
        sleep 0.1
    done
    echo "$what did not come up at $url; see $data/*.log" >&2
    exit 1
}

cleanup() {
    jobs -p | xargs -r kill 2>/dev/null || true
}
trap cleanup EXIT

cargo build --release --locked -q \
    -p firebreak-owner -p firebreak-agent -p firebreak-merchant -p firebreak-attack \
    -p firebreak-demo
if [ ! -x "$root/flame-lib/target/release/flamed" ]; then
    cargo build --release --locked -q --manifest-path "$root/flame-lib/Cargo.toml" \
        -p flamed --features devnet
fi

rm -rf "$data"
mkdir -p "$data"
merchant init
agent init
owner init --genesis-sparks 1000

"$root/scripts/devnet.sh" "$data" > "$data/devnet.log" 2>&1 &
wait_for "The node" "$rpc"
# Started directly, not through the `agent` function: a backgrounded function is a subshell, and
# stopping it at the end would leave the server itself running.
"$bin/firebreak-agent" --data-dir "$data" --rpc "$rpc" \
    serve --listen "$agent_api" --reveal-key > "$data/agent.log" 2>&1 &
wait_for "The app's API" "http://$agent_api/api/status"
"$bin/firebreak-demo" --data-dir "$data" --rpc "$rpc" --agent "http://$agent_api" \
    --listen "$dashboard" > "$data/dashboard.log" 2>&1 &
wait_for "The dashboard" "http://$dashboard/"
printf '\nLOCAL DEVNET running. Dashboard: \033[1mhttp://%s\033[0m\n' "$dashboard"

step "1. A fresh owner wallet of 1,000 sparks, and an allowance of 100 for one merchant"
run owner status
run owner create-allowance \
    --merchant "$(cat "$data/public/merchant-address")" \
    --delegate "$(cat "$data/public/delegate-key")" \
    --vouchers 50,20,20,10 --wait
run agent import "$data"/public/allowance-*.json
run owner status

step "2. The delegated key is revealed; redirecting the money with it fails"
run attack attempt owner-branch
run attack attempt key-path
run attack attempt forged-leaf
run agent status

step "3. The app buys something for 60: it redeems the 50 and the 10"
run agent pay --merchant "$(cat "$data/public/merchant-address")" --amount 60 --wait
refused agent pay --merchant "$(cat "$data/public/merchant-address")" --amount 110

step "4. The merchant opens its receipts and spends what it received"
run merchant inspect
run merchant spend --wait
run merchant inspect

step "5. The owner recovers the two unused 20s"
run owner reclaim --wait
run owner status

step "6. A recovered voucher cannot be redeemed"
recovered="$(owner status --json \
    | jq -r '[.allowances[].vouchers[] | select(.state == "recovered")][0].id')"
run attack attempt redeem-spent --voucher "$recovered"

if [ "$auto" = 1 ]; then
    owner_balance="$(owner status --json | jq -r '.wallet.balance')"
    merchant_balance="$(merchant inspect --json | jq -r '.balance')"
    unspent="$(owner status --json \
        | jq '[.allowances[].vouchers[] | select(.state == "unspent")] | length')"
    echo "owner=$owner_balance merchant=$merchant_balance unspent vouchers=$unspent"
    if [ "$owner_balance" != 940 ] || [ "$merchant_balance" != 60 ] || [ "$unspent" != 0 ]; then
        echo "FAILED: expected owner=940 merchant=60 unspent vouchers=0" >&2
        exit 1
    fi
    echo "OK: owner 940, merchant 60, allowance 0."
else
    read -r -p "Done. Enter stops the node, the app and the dashboard. " _
fi
