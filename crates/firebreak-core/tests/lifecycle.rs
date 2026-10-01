//! The whole voucher lifecycle against a real node: the owner funds an allowance of 100 out of
//! 1,000, the delegate pays 60 to the merchant, the merchant opens the receipts and spends what it
//! received, the owner recovers the unspent 40, and nobody can redeem a recovered voucher.

mod common;

use common::{DENOMINATIONS, Devnet, GENESIS_SPARKS, Parties, fund, output, rng};
use firebreak_core::{WalletInput, build};
use flamekd::util;
use flamepayments::{InputSpec, build_transfer, prepare_output};
use flamevm::{TxEntry, Value};

#[test]
fn an_allowance_pays_the_merchant_and_returns_the_rest_to_the_owner() {
    let mut rng = rng(7);
    let parties = Parties::new(&mut rng);
    let mut devnet = Devnet::new(&parties.owner);
    let allowance = fund(&mut devnet, &parties, &mut rng);
    let vouchers = &allowance.vouchers;
    for voucher in vouchers {
        assert!(devnet.is_unspent(&voucher.id()), "every voucher is funded");
    }
    assert!(devnet.is_spent(&devnet.genesis.id()));
    let change = devnet.received(&parties.owner, util::CHANGE, 0);
    assert_eq!(change.len(), 1);
    assert_eq!(change[0].0.id(), allowance.change.id());
    assert_eq!(
        change[0].1.qty,
        GENESIS_SPARKS - DENOMINATIONS.iter().sum::<u64>()
    );

    // The delegate pays 60 with the 50 and the 10, in one transaction signed by its key alone.
    let paying = [&vouchers[0], &vouchers[3]];
    assert_eq!(paying.iter().map(|voucher| voucher.qty).sum::<u64>(), 60);
    let unsigned = build::redemption(&paying).expect("build the redemption");
    let merchant_predicate = parties.merchant_address().spending_key().compress();
    pays_unchanged(unsigned.log().entries(), &paying, merchant_predicate);
    assert_eq!(unsigned.signing_instructions().items.len(), 2);
    let tx = build::sign(unsigned, &[parties.delegate_key]).expect("sign the redemption");
    let proofs = paying
        .iter()
        .map(|voucher| devnet.proof(&voucher.id()))
        .collect();
    let txid = devnet
        .submit(tx, proofs)
        .expect("the node admits the redemption");
    devnet.confirm(txid);
    assert!(devnet.is_spent(&vouchers[0].id()) && devnet.is_spent(&vouchers[3].id()));
    assert!(devnet.is_unspent(&vouchers[1].id()) && devnet.is_unspent(&vouchers[2].id()));

    // The merchant finds both payouts, opens their receipts, and spends them to a fresh address.
    let received = devnet.received(&parties.merchant, util::RECEIVING, 0);
    let mut amounts: Vec<u64> = received.iter().map(|(_, opening, _)| opening.qty).collect();
    amounts.sort();
    assert_eq!(amounts, [10, 50]);
    let merchant_key = parties
        .merchant
        .spending_key_at(util::RECEIVING, 0)
        .expect("merchant key");
    let inputs: Vec<InputSpec> = received
        .iter()
        .map(|(contract, opening, _)| {
            InputSpec::confidential(
                contract,
                opening,
                devnet.proof(&contract.id()),
                merchant_key,
            )
            .expect("the receipt's opening spends the payout")
        })
        .collect();
    let fresh = parties
        .merchant
        .address_at(util::RECEIVING, 1)
        .expect("fresh address");
    let proofs = inputs.iter().map(|input| input.proof().clone()).collect();
    let unsigned = build_transfer(
        &inputs,
        &[output(fresh, 60)],
        0,
        build::HEADER,
        build::LIMITS,
        &mut rng,
    )
    .expect("build the merchant's spend");
    let tx = flamepayments::sign(unsigned, &[merchant_key, merchant_key]).expect("sign");
    let txid = devnet
        .submit(tx, proofs)
        .expect("the node admits the merchant's spend");
    devnet.confirm(txid);
    let spent = devnet.received(&parties.merchant, util::RECEIVING, 1);
    assert_eq!(spent.len(), 1);
    assert_eq!(spent[0].1.qty, 60);

    // The owner recovers both 20s into one fresh output, with nothing but its own key and the
    // openings it kept.
    let stale = devnet.proof(&vouchers[1].id());
    let back = parties
        .owner
        .address_at(util::CHANGE, 1)
        .expect("recovery address");
    let prepared = prepare_output(&output(back, 40), &mut rng).expect("prepare");
    let recovering = [
        (&vouchers[1], &allowance.openings[1]),
        (&vouchers[2], &allowance.openings[2]),
    ];
    let unsigned = build::recovery(&recovering, back.spending_key().compress(), &prepared, 0)
        .expect("build the recovery");
    let tx = build::sign(unsigned, &[parties.owner_key]).expect("sign the recovery");
    let proofs = vec![
        devnet.proof(&vouchers[1].id()),
        devnet.proof(&vouchers[2].id()),
    ];
    let txid = devnet
        .submit(tx, proofs)
        .expect("the node admits the recovery");
    devnet.confirm(txid);
    let recovered = devnet.received(&parties.owner, util::CHANGE, 1);
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].1.qty, 40);

    // The owner spends what it recovered, which shows the recovered output is an ordinary one.
    let owner_key = parties
        .owner
        .spending_key_at(util::CHANGE, 1)
        .expect("owner key");
    let input = WalletInput::confidential(
        &recovered[0].0,
        &recovered[0].1,
        devnet.proof(&recovered[0].0.id()),
        owner_key,
    )
    .expect("an input");
    let again = parties
        .owner
        .address_at(util::CHANGE, 2)
        .expect("another address");
    let prepared = prepare_output(&output(again, 40), &mut rng).expect("prepare");
    let payouts = [firebreak_core::Payout::Wallet {
        to: again.spending_key().compress(),
        prepared: &prepared,
    }];
    let unsigned =
        build::funding(std::slice::from_ref(&input), &payouts, 0).expect("build the spend");
    let tx = build::sign(unsigned, &[owner_key]).expect("sign");
    let txid = devnet
        .submit(tx, vec![input.proof().clone()])
        .expect("the node admits the owner's spend");
    devnet.confirm(txid);

    // A recovered voucher cannot be redeemed: its last proof no longer proves anything.
    let unsigned = build::redemption(&[&vouchers[1]]).expect("build");
    let tx = build::sign(unsigned, &[parties.delegate_key]).expect("sign");
    let refused = devnet.submit(tx, vec![stale]);
    assert!(
        matches!(refused, Err(flamed::NodeError::Mempool(_))),
        "the mempool refuses a spent voucher: {refused:?}"
    );

    // Owner 900 + 40, merchant 60, allowance 0.
    let owner: u64 = [
        devnet.received(&parties.owner, util::CHANGE, 0),
        devnet.received(&parties.owner, util::CHANGE, 2),
    ]
    .iter()
    .flatten()
    .filter(|(_, _, spent)| !spent)
    .map(|(_, opening, _)| opening.qty)
    .sum();
    let merchant: u64 = devnet
        .received(&parties.merchant, util::RECEIVING, 1)
        .iter()
        .filter(|(_, _, spent)| !spent)
        .map(|(_, opening, _)| opening.qty)
        .sum();
    assert_eq!(
        owner,
        GENESIS_SPARKS - DENOMINATIONS.iter().sum::<u64>() + 40
    );
    assert_eq!(owner, 940);
    assert_eq!(merchant, 60);
    assert!(
        vouchers
            .iter()
            .all(|voucher| devnet.is_spent(&voucher.id()))
    );
}

/// Checks that each voucher became exactly: its input, the unchanged token under the merchant's
/// spending key, and its receipt in the very next entry.
fn pays_unchanged(
    entries: &[TxEntry],
    paying: &[&firebreak_core::Voucher],
    merchant: curve25519_dalek::ristretto::CompressedRistretto,
) {
    let effects: Vec<&TxEntry> = entries
        .iter()
        .filter(|entry| !matches!(entry, TxEntry::Header(_) | TxEntry::CellWitness(_)))
        .collect();
    assert_eq!(
        effects.len(),
        3 * paying.len(),
        "input, output and receipt per voucher"
    );
    for (voucher, effects) in paying.iter().zip(effects.chunks(3)) {
        let [
            TxEntry::Input(id),
            TxEntry::Output(payout),
            TxEntry::Data(receipt),
        ] = effects
        else {
            panic!("unexpected effects {effects:?}");
        };
        assert_eq!(*id, voucher.id());
        assert_eq!(payout.predicate.to_point(), merchant);
        let (Value::Token(paid), Value::Dict(payload)) =
            (payout.payload(), voucher.contract.payload())
        else {
            panic!("the payout holds a token");
        };
        let Some(Value::Token(locked)) = payload.get(&flamevm::Scalar::from(0u64)) else {
            panic!("the voucher holds a token");
        };
        assert_eq!(
            paid.qty().to_point(),
            locked.qty().to_point(),
            "the same quantity"
        );
        assert_eq!(
            paid.flv().to_point(),
            locked.flv().to_point(),
            "the same flavor"
        );
        assert_eq!(*receipt, voucher.receipt().expect("a receipt"));
    }
}
