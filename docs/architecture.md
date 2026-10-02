# Architecture

This document describes the transactions Firebreak builds, what each role holds, and how they
exchange data. For what the design protects against, see [threat-model.md](threat-model.md).

## Roles and their data

```text
                       public/allowance-<id>.json
   firebreak-owner  ───────────────────────────────▶  firebreak-agent  ◀── HTTP /api/pay
   owner/owner.json      (delegation package:          agent/agent.json      (dashboard,
   wallet seed,           policies and contracts,       delegated key,         or an AI tool)
   authority key,         no owner secrets)             voucher states
   voucher openings
        │                                                    │
        │ fund, recover                                       │ redeem
        ▼                                                    ▼
   ┌──────────────────────────── flamed (local devnet, JSON-RPC) ───────────────────────────┐
   └──────────────────────────────────────────────────────────────────────────────────────────┘
        ▲                                                    ▲
        │ scan, spend                                         │ adversarial candidates
   firebreak-merchant                                   firebreak-attack
   merchant/merchant.json                               delegated key + public packages only
   merchant seed, receipts

   public/journal.jsonl, public/*-status.json  ──▶  firebreak-demo (read-only dashboard)
```

Every role is a separate process with its own key file (owner-only permissions) under
`.firebreak/<role>/`. Anything under `.firebreak/public/` holds no secret: the delegation packages,
the merchant's address, the delegate's public key, the owner's and merchant's status snapshots for
the dashboard, and the journal of every submitted transaction.

## The voucher

A voucher is a contract: a predicate, an anchor, and one payload value.

- **Payload**: a dictionary `{0: token, 1: receipt}`. The token is a confidential Flame token
  (Pedersen commitments to quantity and flavor). The receipt is the payment note that the
  merchant's wallet will need to open the token once it is paid out: Flame's own note format
  (`0x01 ‖ R ‖ AES-SIV tag ‖ ciphertext`), sealed to the merchant's address with fresh sender
  randomness by `flamepayments::prepare_output`. The token's blinding factors are derived from that
  same exchange, so the receipt describes exactly this token.
- **Predicate**: `P = X + H(X, root)·B`, a Taproot-style tree with Flame's unspendable internal key
  `X`, over two programs and per-voucher random blinding leaves.

Redemption program (delegated):

```text
push 0, push delegate_vk, contract, signtx, drop    # the delegated key authorizes the tx
push 0, get, roll 1, drop                           # take the token
push merchant_S, output                             # pay it, whole, to the merchant
push 1, get, roll 1, drop, log                      # log the fixed receipt right after
drop                                                # drop the emptied payload; return nothing
```

Recovery program (owner):

```text
push 0, push owner_vk, contract, signtx, drop       # the owner's key authorizes the tx
push 1, get, roll 1, drop, drop                     # discard the receipt
push 0, get, roll 1, drop, roll 1, drop             # take the token, drop the emptied payload
push 1, return                                      # hand the token to the owner's transaction
```

The authorization idiom locks a throwaway scalar under the key with `contract` and unlocks it
with `signtx`, which records a signature requirement on the key, bound to the transaction ID.
Neither program takes arguments, and a program fails if it ends with anything left on its stack.

## Transactions

All three are external transactions with zero fees. The script, then the effects it produces.

**Funding** (owner, `build::funding`): spend wallet outputs, `mix` them into the voucher tokens
and the change, then lock each voucher token with its receipt under its predicate.

```text
per input:    push contract (with openings), input, signtx
              push commitments of every payout, push m, push n, mix
per voucher:  roll, push 0, push receipt, push 1, push 2, dict, push P, output
change:       roll, push S_owner, output, push note, log
effects:      Input…, Output(voucher)…, Output(change), Data(note)
```

**Redemption** (delegate, `build::redemption`), per voucher:

```text
push voucher, input, push taproot proof (redeem), push gas, push 0, open, verify, drop
effects:      Input(voucher), Output(token → merchant S), Data(receipt)
```

The merchant's wallet sees an ordinary payment: an output under its spending key with the note in
the next log entry. It opens it with `flamepayments::open_note`, which also checks that the note's
opening rebuilds the published token, and spends it like any other output.

**Recovery** (owner, `build::recovery`):

```text
per voucher:  push voucher (with opening), input, push taproot proof (recover), push gas,
              push 0, open, verify, drop
              push commitments, push m, push 1, mix, push S_owner, output, push note, log
effects:      Input(voucher)…, Output(owner), Data(note)
```

Signing: the VM returns one signature requirement per `signtx`, in execution order, each naming its
key. The signer provides one key per requirement, in that order, repeating the delegated key once
per voucher, and Flame aggregates them into one multi-signature over the transaction ID.

## Voucher states

Each role records a state per voucher and reconciles it with the node (`store::reconcile`):

| State | Meaning |
| --- | --- |
| `prepared` | The owner holds the voucher's opening; its funding is not submitted |
| `funding_pending` | The funding transaction is in the mempool |
| `unspent` | Funded and unspent |
| `redemption_pending` | A redemption is in the mempool |
| `recovery_pending` | A recovery is in the mempool |
| `redeemed` | A confirmed redemption paid the merchant |
| `recovered` | A confirmed recovery paid the owner |
| `unknown` | Not enough information; resynchronize before acting |

Only the node decides that a voucher is spent, and it reports a spend only once a block holds it.
A pending state lasts while its transaction waits in the mempool. A lost reply from the node is an
unknown outcome, never a failure: the roles check the transaction and the voucher before they
build a replacement. The owner saves every voucher's opening before it submits the funding, so a
crash cannot leave a funded voucher it cannot recover.

## Membership proofs

Spending a contract requires a Utreexo membership proof that is valid at the node's current tip,
and every block that holds transactions changes the proofs. The roles fetch proofs immediately
before packaging a transaction. If the node refuses one as stale, they fetch fresh proofs and
rebuild once; a voucher that turns out to be spent is reported as such.
