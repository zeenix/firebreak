# Firebreak

Limited payment authority for apps on [Flame](https://github.com/runflame/flame-lib).

An owner gives an app a delegated key and an allowance. The app can pay one merchant, up to the
allowance, and nothing else: it cannot redirect the money, extract change, or pay fees with it. The
amounts stay confidential on the public chain, the merchant can open its receipts and spend what it
received, and the owner can take back whatever the app has not spent, at any time, with its own key
alone.

Firebreak is a hackathon prototype for the Build on Flame challenge at btc++ Berlin 2026. It runs
on a **local Flame devnet** only.

## How it works

An allowance is a set of **fixed-value, single-use vouchers**. An allowance of 100 sparks can be
vouchers of 50, 20, 20 and 10: paying 60 redeems the 50 and the 10, and the owner can recover the
two 20s.

Each voucher is a Flame contract that locks two things: a confidential token holding the voucher's
value, and a receipt, which is the payment note the merchant will need, sealed to the merchant
before the voucher is funded. The contract's predicate is a scripts-only Taproot tree with no key
path and exactly two programs:

- **Redeem**: requires a transaction-bound signature by the delegated key, outputs the whole token
  to the merchant's spending key, and logs the fixed receipt right after it. It takes no arguments:
  the destination, amount and receipt are all fixed at funding time.
- **Recover**: requires a transaction-bound signature by the owner's key, discards the receipt, and
  hands the token to the owner's transaction, which pays it back into the owner's wallet.

After a redemption, the merchant's ordinary wallet finds the output, opens the receipt with its
viewing key, checks it against the token's commitments, and spends it like any other payment. See
[docs/threat-model.md](docs/threat-model.md) for the adversary, the invariant, the evidence for
each claim, and what Firebreak does not claim.

### Why vouchers instead of arbitrary amounts

Flame validates token commitments and spending rules, but a malformed encrypted payment note is not
invalid at the chain layer. If the untrusted app chose fresh change outputs and their notes during
a payment, it could make funds unrecoverable. Fixing every voucher's destination and receipt before
delegation removes that hazard. The price is denominations: a payment must match a subset of the
remaining vouchers exactly. A payment that has no exact subset is a limitation of the
denominations, not a cryptographic rejection.

## Components

| Crate | Role |
| --- | --- |
| `firebreak-core` | Voucher programs and predicate tree, funding, redemption and recovery transactions, signing, node client, stores, journal |
| `firebreak-owner` | Owner CLI: holds the owner's wallet and voucher openings; creates and recovers allowances |
| `firebreak-agent` | The app: holds only the delegated key; pays from the allowance through a narrow API |
| `firebreak-merchant` | Merchant CLI: finds payments, opens receipts, spends what it received |
| `firebreak-attack` | Adversary: builds and submits unauthorized transactions with the delegated key |
| `firebreak-demo` | Local dashboard: owner, app, merchant and public-observer views |
| `firebreak-mcp` | Optional AI tool: an MCP server that offers a model only the app's `pay` (see [docs/ai-tool.md](docs/ai-tool.md)) |

Every component that touches the chain uses Flame: `flamevm` for the contract, scripts, proofs and
signatures, `flamepayments` for keys, notes and ordinary transfers, `flamechain` for transaction
packaging and membership proofs, and `flamed` as the node, over its JSON-RPC interface.

## Building

Firebreak builds against a pinned Flame revision through local path dependencies.

```sh
scripts/setup-flame.sh        # clones flame-lib at the pinned revision and applies the local patch
cargo build --release
cargo build --release --manifest-path flame-lib/Cargo.toml -p flamed --features devnet
```

- Flame revision: `8021a125da8febe0223a0f00e407887bd60d136f` of
  [runflame/flame-lib](https://github.com/runflame/flame-lib) (the toolchain, 1.90.0, and the
  dependency versions in `Cargo.lock` are Flame's own).
- Local patch: [`patches/flame-lib/0001-flamepayments-prepare-output.patch`](patches/flame-lib/0001-flamepayments-prepare-output.patch)
  adds `flamepayments::prepare_output`, which seals an output's note with Flame's existing note
  code ahead of the transaction that publishes the output. It exposes no shared secret and adds no
  new encryption.

## Running the demonstration

```sh
scripts/demo.sh          # step by step, pausing before each step
scripts/demo.sh --auto   # the same without pauses; checks the final balances
```

The script wipes `./.firebreak`, creates fresh keys for the three parties, writes a devnet genesis
that gives the owner 1,000 sparks, starts `flamed` (2-second blocks, zero fees), the app's API and
the dashboard at <http://127.0.0.1:7742>, and then runs the story with the real binaries:

1. The owner funds an allowance of 100 for one merchant: vouchers of 50, 20, 20 and 10.
2. The delegated key is revealed, and the attacker tries to redirect the money with it. Every
   attempt is refused by the prover or by the node, and the vouchers stay unspent.
3. The app pays 60: it redeems the 50 and the 10. Asking for 110 is refused.
4. The merchant opens its receipts, checks them against the payments, and spends the 60.
5. The owner recovers the two unused 20s.
6. Redeeming a recovered voucher is refused by the node.

The public observer view shows what anyone watching the chain sees: transaction IDs, inputs,
outputs, commitments and receipt lengths, with every amount marked "not public". The final
balances are owner 940, merchant 60, allowance 0.

Each role is its own program with its own key file under `.firebreak/<role>/`:
`firebreak-owner`, `firebreak-agent`, `firebreak-merchant`, and the adversary's
`firebreak-attack`. For example:

```sh
firebreak-attack list                        # what the adversary can try
firebreak-attack attempt owner-branch        # one attack, with where it was stopped
firebreak-attack matrix                      # every attack that must be refused
firebreak-attack attempt key-path --key HEX  # with the key the dashboard revealed
```

## Testing

```sh
cargo test
```

The tests run every transaction against a real, in-process Flame node: the bytes are serialized,
decoded and verified the way the chain does, then admitted (or refused) by the node's mempool and
confirmed in blocks.

- `crates/firebreak-core/tests/lifecycle.rs`: fund 50, 20, 20 and 10 out of 1,000; redeem 60 with
  the delegated key alone; the merchant opens the receipts and spends 60; the owner recovers 40
  with its own key alone and spends it; a recovered voucher cannot be redeemed; final balances are
  owner 940, merchant 60, allowance 0.
- `crates/firebreak-attack/tests/matrix.rs`: the attack matrix. See
  [docs/threat-model.md](docs/threat-model.md).

## License and scope of original work

Flame is Apache-2.0 and is not vendored: `scripts/setup-flame.sh` fetches it, and the only change
to it is the patch above. Everything under `crates/`, `scripts/`, `patches/` and `docs/` is original
work for this hackathon.
