# Threat model

Firebreak gives an app limited payment authority on Flame. This document states what that
authority is, who is trusted, what an adversary holding the app's key can and cannot do, and what
Firebreak does not claim. Each claim names the test that checks it.

## Parties and keys

| Party | Holds | Trusted for |
| --- | --- | --- |
| Owner | Wallet seed, voucher authority key, every voucher's opening | Everything: it funds, defines and recovers allowances |
| Delegate (the app) | Delegated key, public voucher descriptors | Nothing. Assumed hostile |
| Merchant | Merchant wallet seed | Receiving payments; not trusted to deliver goods |
| Public | Chain data | Nothing |

The owner's funding and recovery signer is the trusted component. It chooses the merchant, the
delegated key, the voucher amounts, the contract programs and the encrypted receipts.

## The adversary

The adversary has the delegated private key, every public voucher descriptor (including the data
needed to rebuild each voucher's predicate tree), and direct access to the node's transaction
submission. It can build its own programs, alter serialized transactions, add inputs and outputs,
and try either branch of a voucher. It has neither the owner's key nor the merchant's spending key.

## The invariant

For each voucher, either its unchanged token reaches the fixed merchant under delegated
authorization, or the owner authorizes recovery. No other party obtains its payload.

How the contract enforces it (see `crates/firebreak-core/src/voucher.rs`):

- A voucher's predicate is a scripts-only Taproot tree whose internal key is Flame's unspendable
  key, so no key path exists: `signtx` or `signcall` on the voucher itself cannot be satisfied.
- The tree commits to exactly two programs and a per-voucher random blinding. Opening the voucher
  with any other program fails the Taproot commitment check.
- The redemption program takes no arguments. It requires a transaction-bound signature by the
  delegated key, outputs the voucher's whole token to the merchant's spending key, logs the receipt
  fixed in the voucher's payload in the very next log entry, and returns nothing to its caller.
- The recovery program requires a transaction-bound signature by the owner's key and returns the
  token to the transaction that the owner's signature binds as a whole.
- Both signatures are made inside the program by locking a throwaway scalar under the key and
  unlocking it with `signtx`, which records a requirement bound to the transaction ID. The
  aggregate signature therefore covers every effect of the transaction.

The allowance limit is the sum of the funded vouchers, not a counter on a server. Conservation of
value (`mix`), single-use contract inputs and the fixed programs enforce it on chain.

## What the adversary cannot do

Every row is run by `crates/firebreak-attack/tests/matrix.rs` (or the lifecycle test named) against
an in-process Flame node, with the genuine delegated key. Each candidate is serialized, decoded
and verified the way the chain does, then offered to the node's mempool. After every refused
attempt the test mints a block and checks that the targeted voucher is still unspent, and at the
end that it still redeems to the merchant.

| Attempt | Stopped by | Observed result |
| --- | --- | --- |
| Spend the voucher through `signtx` on its own predicate (key path), signed by the delegate | Verifier and node | `BatchSignatureVerificationFailed` |
| Open the owner's recovery branch and pay the token to the attacker, signed by the delegate | Verifier and node | `BatchSignatureVerificationFailed` |
| Open the voucher with a redemption leaf that pays the attacker | Prover | `TaprootProofMismatch` |
| Open the voucher with a redemption leaf without the delegated authorization | Prover | `TaprootProofMismatch` |
| Open the genuine redemption branch with an extra, payload-shaped argument | Prover | The branch cannot end with the real payload still on its stack, so it fails; `open` returns the voucher locked and the required success flag fails: `VerifyFailed` |
| Submit a genuine redemption with its signature stripped | Verifier and node | `MissingTxBoundSignature` |
| Pay a fee in the same transaction as a redemption | Prover | `StackNotClean`: the fee debt has nothing to balance against, because the branch paid the whole token out |
| Redeem one voucher twice in one transaction | Node | Mempool refuses the second membership proof (`Utreexo(InvalidProof)`) |
| Redeem a voucher after its recovery confirmed | Node | Mempool refuses the spent input |
| Log a forged receipt after a genuine redemption | Not stopped: a valid payment | The entry after the merchant's output is still the fixed receipt; the merchant opens it |
| Race a redemption against a recovery of the same voucher | Node | Exactly one confirms; the other is refused (`Utreexo(InvalidProof)`) |

"Prover" means the VM refused to prove the candidate, so no transaction bytes exist to submit.
That is a real rejection, but a weaker form of evidence than a node refusing bytes: the tables
above keep the two apart, and none of the prover-stage rows is presented as a node decision.

The honest client also refuses a payment it cannot make exactly (for example 110 from an
allowance of 100). That is a policy check of the client and proves nothing about security; the
on-chain evidence for the cap is the conservation and single-use behavior above.

## Confidentiality

Amounts and receipt contents are hidden from public observers: tokens are Pedersen commitments and
receipts are AES-SIV encrypted to the merchant's viewing key. `public_bytes_carry_no_opening`
checks that no blinding factor appears in the submitted bytes.

What remains public: the transaction graph, which contracts are spent and created, output
predicates, output and log-entry counts and sizes, and the fact that a redemption moves a
voucher's token unchanged (its commitments are the same before and after). Firebreak is not
anonymous or unlinkable.

The merchant can read a voucher's amount before redemption, because the receipt in the voucher's
payload is encrypted to it. The promise is confidentiality from the public, not from the intended
merchant.

## Revocation

Recovery races with a valid delegated redemption until one of them confirms. Firebreak reports a
voucher as recovered, and the authority as revoked, only once the recovery has confirmed; until
then it reports "recovery pending".

## Fees

A voucher cannot pay fees: its value leaves only through one of the two programs. Firebreak runs
on a local devnet with a minimum fee of zero, so its transactions pay none. On a network with fees,
a redemption would need a separate fee input with its own balancing, and the owner would pay
recovery fees from its wallet; neither is implemented.

## Not claimed

- Protection against a compromised owner signer, a compromised host, or a merchant that colludes
  with the delegate.
- Protection against the delegate spending the entire allowance at the authorized merchant: that
  is what the allowance authorizes.
- That a merchant delivers anything after being paid.
- Hardened isolation between roles. The roles run as separate processes with separate key files,
  which is a logical boundary on a shared laptop, not an operating-system security boundary.
- Production key storage. Keys and openings are plain JSON files with owner-only permissions.
- Anything about a network other than the local devnet.
