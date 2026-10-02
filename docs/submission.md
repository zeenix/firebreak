# Submission notes

Firebreak, for the Build on Flame challenge at btc++ Berlin 2026: limited payment authority for
apps on Flame, through fixed-value, single-use vouchers.

## Three observable results

1. **Unauthorized redirection fails.** With the delegated key revealed, the attacker builds its
   own transactions and submits them straight to the node, bypassing the app. Paying itself
   through the owner's branch, spending the voucher as a key, or opening it with a leaf that pays
   the attacker are all refused, with the VM's or the node's own error, and the voucher stays
   unspent: `firebreak-attack attempt owner-branch`, `key-path`, `forged-leaf`, or `matrix` for
   all of them.
2. **The confidential payment succeeds.** The app pays 60 by redeeming the 50 and the 10. The
   public observer sees transaction IDs, inputs, outputs, commitments and receipt lengths, never an
   amount. The merchant opens its receipts, checks them against the tokens, and spends the 60.
3. **Unused authority can be recovered.** The owner, with its own key alone, recovers the two 20s
   into its wallet, and a recovered voucher cannot be redeemed. Final balances: owner 940,
   merchant 60, allowance 0, out of 1,000; the local devnet charges no fees.

## Where Flame is used

- The voucher contract is a FlameVM contract: a scripts-only Taproot predicate with two programs,
  built and proven with `flamevm`, signed with Flame's aggregate transaction-bound signatures.
- Notes, keys, addresses and ordinary transfers come from `flamepayments` and `flamekd`, with one
  local patch: `prepare_output`, which seals an output's note ahead of the transaction that
  publishes it, using Flame's existing note code.
- Transactions are packaged with `flamechain` (with Utreexo membership proofs) and submitted to
  `flamed`, run as a local devnet, over its JSON-RPC interface.

## Reproducing it

- Flame revision `8021a125da8febe0223a0f00e407887bd60d136f` of runflame/flame-lib, fetched and
  patched by `scripts/setup-flame.sh`. The patch is
  `patches/flame-lib/0001-flamepayments-prepare-output.patch`.
- Build: see the README. Run: `scripts/demo.sh` (step by step) or `scripts/demo.sh --auto`.
- Every key in a run is fresh and disposable. Nothing in the repository is a key: runtime keys,
  openings and the owner's store live in `.firebreak/`, which git ignores.

## Checklist

Done:

- [x] Record the revision actually used, including local modifications (README, this file).
- [x] Run the demo from clean application state with real validation: `scripts/demo.sh --auto`
      against the real `flamed`, with release builds, ends with owner 940, merchant 60,
      allowance 0.
- [x] Run `scripts/demo.sh --auto` once more on the presenting laptop, from a fresh checkout: same
      result.
- [x] Confirm merchant spendability and owner-only recovery (lifecycle tests and the demo).
- [x] Review the attack evidence and the wording of security claims (docs/threat-model.md).
- [x] Reconcile the final balances; the devnet charges no fees (owner 940, merchant 60).

Still to do by the team:

- [ ] Confirm the Flame bonus deadline and whether registration in the shared project system is
      required.
- [ ] Confirm that a local devnet demonstration is accepted, and any sponsor-specific
      deliverables.
- [ ] Record a short run of `scripts/demo.sh` as backup evidence, labelled as a recording; the
      live dashboard always shows the current state.
- [ ] Verify the submission links and keep the recording available.
