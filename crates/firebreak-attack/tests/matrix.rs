//! The attack matrix, run against a real in-process node.
//!
//! Every candidate is built and signed by the adversary with the genuine delegated key, then
//! serialized, decoded and verified the way the chain does, and offered to the node's mempool. A
//! test asserts the stage that actually stopped each candidate, and that the targeted voucher is
//! still unspent and still redeemable afterwards.

use firebreak_attack::{Adversary, Attack, Candidate, Stopped, craft, forged_receipt};
use firebreak_core::devnet::{Allowance, Devnet, Parties, fund, output, rng};
use firebreak_core::{Voucher, build, keys};
use flamechain::utreexo::Proof;
use flamechain::{BlockTx, ChainParams};
use flamed::{NodeError, ProofStatus, TxStatus};
use flamekd::{Network, util};
use flamepayments::{Account, open_note, prepare_output};
use flamevm::{TxID, VMError};
use rand::rngs::StdRng;

/// What happened to one candidate.
#[derive(Debug)]
enum Verdict {
    /// The prover refused to prove it.
    Prover(VMError),
    /// The bytes existed and the node refused them. `verifier` is what a standalone verification
    /// of the same bytes said first.
    Refused {
        verifier: Option<VMError>,
        node: NodeError,
    },
    /// The node admitted it.
    Accepted(TxID),
}

struct Setup {
    devnet: Devnet,
    parties: Parties,
    allowance: Allowance,
    adversary: Adversary,
    rng: StdRng,
}

fn setup(seed: u64) -> Setup {
    let mut rng = rng(seed);
    let parties = Parties::new(&mut rng);
    let mut devnet = Devnet::new(&parties.owner);
    let allowance = fund(&mut devnet, &parties, &mut rng);
    let attacker = Account::from_seed(&keys::random_bytes(&mut rng), Network::Testnet, 0)
        .expect("an attacker account");
    let adversary = Adversary {
        delegate_key: parties.delegate_key,
        address: attacker
            .address_at(util::RECEIVING, 0)
            .expect("attacker address"),
    };
    Setup {
        devnet,
        parties,
        allowance,
        adversary,
        rng,
    }
}

/// Crafts `attack` and offers it to the node, with the current proof of every input that has one
/// and `stale` for any that does not.
fn attempt(
    devnet: &mut Devnet,
    attack: Attack,
    targets: &[&Voucher],
    adversary: &Adversary,
    stale: Option<&Proof>,
) -> Verdict {
    let Candidate { tx, inputs } = match craft(attack, targets, adversary) {
        Ok(candidate) => candidate,
        Err(Stopped::Prover(error)) => return Verdict::Prover(error),
        Err(other) => panic!("{}: unexpected refusal: {other}", attack.name()),
    };
    let proofs = inputs
        .iter()
        .map(|id| match devnet.node.proof(id) {
            ProofStatus::Unspent(proof) => proof,
            _ => stale.cloned().expect("a stale proof for a spent input"),
        })
        .collect();
    let bytes = build::package(tx, proofs).expect("package");
    let params = ChainParams::default();
    let verifier = BlockTx::from_bytes_bounded(&bytes, params.version, params.limits)
        .expect("the candidate decodes")
        .tx
        .verify(build::LIMITS)
        .err();
    match devnet.node.submit(&bytes) {
        Ok(txid) => Verdict::Accepted(txid),
        Err(node) => Verdict::Refused { verifier, node },
    }
}

#[test]
fn every_unauthorized_spend_leaves_the_voucher_locked() {
    let Setup {
        mut devnet,
        parties,
        allowance,
        adversary,
        ..
    } = setup(11);
    let target = &allowance.vouchers[0];

    for attack in [
        Attack::KeyPath,
        Attack::OwnerBranch,
        Attack::ForgedLeaf,
        Attack::StrippedLeaf,
        Attack::ExtraArgument,
        Attack::MissingSignature,
        Attack::FeeSiphon,
        Attack::DuplicateInput,
    ] {
        let verdict = attempt(&mut devnet, attack, &[target], &adversary, None);
        println!("{}: {verdict:?}", attack.name());
        match (attack, &verdict) {
            // A signature by the delegated key cannot stand in for the voucher's own predicate
            // key, which nobody knows, or for the owner's key.
            (Attack::KeyPath | Attack::OwnerBranch, Verdict::Refused { verifier, .. }) => {
                assert!(
                    matches!(verifier, Some(VMError::BatchSignatureVerificationFailed)),
                    "{}: {verdict:?}",
                    attack.name()
                );
            }
            // A changed leaf is a different tree: the predicate does not open to it.
            (Attack::ForgedLeaf | Attack::StrippedLeaf, Verdict::Prover(error)) => {
                assert!(
                    matches!(error, VMError::TaprootProofMismatch),
                    "{}: {error:?}",
                    attack.name()
                );
            }
            // The branch fails with the payload still on its stack, so `open` hands the voucher
            // back locked and the required success flag is zero.
            (Attack::ExtraArgument, Verdict::Prover(error)) => {
                assert!(matches!(error, VMError::VerifyFailed), "{error:?}");
            }
            (Attack::MissingSignature, Verdict::Refused { verifier, .. }) => {
                assert!(
                    matches!(verifier, Some(VMError::MissingTxBoundSignature)),
                    "{verdict:?}"
                );
            }
            // The branch pays the whole token to the merchant inside its own frame; the fee's
            // debt has nothing to balance against.
            (Attack::FeeSiphon, Verdict::Prover(_)) => {}
            // The VM executes both inputs; the chain refuses to spend one contract twice.
            (Attack::DuplicateInput, Verdict::Refused { node, .. }) => {
                assert!(matches!(node, NodeError::Mempool(_)), "{node:?}");
            }
            _ => panic!("{}: unexpected verdict {verdict:?}", attack.name()),
        }
        devnet.node.mint_block().expect("mint");
        assert!(
            devnet.is_unspent(&target.id()),
            "{} spent it",
            attack.name()
        );
    }

    // After all of it, the voucher still pays the merchant through its genuine branch.
    let tx = build::sign(
        build::redemption(&[target]).expect("build"),
        &[parties.delegate_key],
    )
    .expect("sign");
    let txid = devnet
        .submit(tx, vec![devnet.proof(&target.id())])
        .expect("the genuine redemption is admitted");
    devnet.confirm(txid);
    assert!(devnet.is_spent(&target.id()));
}

#[test]
fn a_recovered_voucher_cannot_be_redeemed() {
    let Setup {
        mut devnet,
        parties,
        allowance,
        adversary,
        mut rng,
    } = setup(12);
    let target = &allowance.vouchers[1];
    let stale = devnet.proof(&target.id());

    let back = parties.owner.address_at(util::CHANGE, 1).expect("address");
    let prepared = prepare_output(&output(back, target.qty), &mut rng).expect("prepare");
    let unsigned = build::recovery(
        &[(target, &allowance.openings[1])],
        back.spending_key().compress(),
        &prepared,
        0,
    )
    .expect("build the recovery");
    let tx = build::sign(unsigned, &[parties.owner_key]).expect("sign");
    let txid = devnet
        .submit(tx, vec![stale.clone()])
        .expect("the recovery is admitted");
    devnet.confirm(txid);

    let verdict = attempt(
        &mut devnet,
        Attack::RedeemSpent,
        &[target],
        &adversary,
        Some(&stale),
    );
    let Verdict::Refused {
        verifier: None,
        node: NodeError::Mempool(_),
    } = verdict
    else {
        panic!("a recovered voucher must be refused by the chain, not the VM: {verdict:?}");
    };
}

#[test]
fn a_forged_receipt_does_not_displace_the_real_one() {
    let Setup {
        mut devnet,
        parties,
        allowance,
        adversary,
        ..
    } = setup(13);
    let target = &allowance.vouchers[0];
    let Verdict::Accepted(txid) = attempt(
        &mut devnet,
        Attack::ReceiptSwap,
        &[target],
        &adversary,
        None,
    ) else {
        panic!("a redemption with an extra log entry is a valid payment");
    };
    devnet.confirm(txid);

    let merchant = parties.merchant_address();
    let view_key = parties
        .merchant
        .viewing_key_at(util::RECEIVING, 0)
        .expect("view key");
    let hits = devnet
        .node
        .scan(&[merchant.spending_key().compress().to_bytes()], 0);
    assert_eq!(hits.len(), 1);
    let note = hits[0].note.as_ref().expect("a receipt").0.clone();
    assert_eq!(note, target.receipt().expect("the receipt"));
    assert_ne!(note, forged_receipt(target).expect("the forgery"));
    let contract = flamechain::codec::contract_from_bytes(&hits[0].bytes.0).expect("a contract");
    let received = open_note(&contract, Some(&note), &merchant, &view_key).expect("it opens");
    assert_eq!(received.opening.qty, target.qty);
}

#[test]
fn redemption_and_recovery_race_and_exactly_one_wins() {
    let Setup {
        mut devnet,
        parties,
        allowance,
        mut rng,
        ..
    } = setup(14);
    let target = &allowance.vouchers[2];
    let proof = devnet.proof(&target.id());

    let redemption = build::sign(
        build::redemption(&[target]).expect("build"),
        &[parties.delegate_key],
    )
    .expect("sign");
    let back = parties.owner.address_at(util::CHANGE, 1).expect("address");
    let prepared = prepare_output(&output(back, target.qty), &mut rng).expect("prepare");
    let recovery = build::sign(
        build::recovery(
            &[(target, &allowance.openings[2])],
            back.spending_key().compress(),
            &prepared,
            0,
        )
        .expect("build"),
        &[parties.owner_key],
    )
    .expect("sign");

    let first = devnet.submit(redemption, vec![proof.clone()]);
    let second = devnet.submit(recovery, vec![proof]);
    devnet.node.mint_block().expect("mint");
    let confirmed: Vec<&TxID> = [&first, &second]
        .into_iter()
        .filter_map(|result| result.as_ref().ok())
        .filter(|txid| matches!(devnet.node.tx_status(txid), TxStatus::Confirmed { .. }))
        .collect();
    println!("redemption: {first:?}, recovery: {second:?}");
    assert_eq!(
        confirmed.len(),
        1,
        "exactly one spend of the voucher confirms"
    );
    assert!(devnet.is_spent(&target.id()));
}

#[test]
fn public_bytes_carry_no_opening() {
    let Setup {
        mut devnet,
        parties,
        allowance,
        ..
    } = setup(15);
    let paying = [&allowance.vouchers[0], &allowance.vouchers[3]];
    let tx = build::sign(
        build::redemption(&paying).expect("build"),
        &[parties.delegate_key],
    )
    .expect("sign");
    let proofs = paying
        .iter()
        .map(|voucher| devnet.proof(&voucher.id()))
        .collect();
    let bytes = build::package(tx, proofs).expect("package");
    for opening in &allowance.openings {
        for secret in [opening.qty_blinding, opening.flv_blinding] {
            assert!(
                !bytes.windows(32).any(|window| window == secret.as_bytes()),
                "a blinding factor is public"
            );
        }
    }
    for voucher in &allowance.vouchers {
        let contract = flamechain::codec::contract_bytes(&voucher.contract).expect("bytes");
        for opening in &allowance.openings {
            assert!(
                !contract
                    .windows(32)
                    .any(|window| window == opening.qty_blinding.as_bytes())
            );
        }
    }
    assert!(devnet.node.submit(&bytes).is_ok());
}
