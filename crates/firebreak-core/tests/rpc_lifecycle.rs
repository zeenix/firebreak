//! The voucher lifecycle through the node's JSON-RPC interface, the way a role's program drives
//! it: everything a transaction needs comes from the node through `Chain`, every transaction is
//! submitted through it, and every answer is checked against the chain's own history.
//!
//! The owner funds an allowance of 100 out of 1,000 from the genesis allocation, the delegate
//! redeems the 50 and the 10, the merchant finds both payments and spends them, and a redemption
//! built on a proof that a block has since made stale is refused until its proof is refreshed.

use std::time::Duration;

use firebreak_core::chain::{Chain, ChainError, ContractState, TxState};
use firebreak_core::devnet::{DENOMINATIONS, GENESIS_SPARKS, LocalNode, Parties, output, rng};
use firebreak_core::wallet;
use firebreak_core::{NETWORK, Payout, Voucher, VoucherPolicy, build};
use flamed_rpc::codes;
use flamekd::util;
use flamepayments::{OutputSpec, PreparedOutput, open_note, prepare_output};
use flamevm::TxID;
use tokio::time;

const MEMO: &[u8] = b"firebreak voucher";

/// How long a test waits for a block it has just minted to show up.
const PATIENCE: Duration = Duration::from_secs(10);

/// Mints a block and waits until `txid` is in it.
async fn confirm(node: &LocalNode, chain: &Chain, txid: &TxID) -> u64 {
    node.mint();
    chain
        .wait_confirmed(txid, PATIENCE)
        .await
        .expect("the transaction confirms")
}

/// Builds, signs and submits a redemption of `vouchers` with the proofs given, and returns what
/// the node says.
async fn redeem(
    chain: &Chain,
    parties: &Parties,
    vouchers: &[&Voucher],
    proofs: Vec<flamechain::utreexo::Proof>,
) -> Result<TxID, ChainError> {
    let unsigned = build::redemption(vouchers).expect("build the redemption");
    let tx = build::sign(unsigned, &[parties.delegate_key]).expect("sign the redemption");
    let bytes = build::package(tx, proofs).expect("package the redemption");
    chain.submit(bytes).await
}

#[tokio::test]
async fn an_allowance_goes_through_the_node_from_funding_to_the_merchants_spend() {
    let mut rng = rng(21);
    let parties = Parties::new(&mut rng);
    let owner_address = parties
        .owner
        .address_at(util::RECEIVING, 0)
        .expect("owner address");
    let merchant = parties.merchant_address();

    let node = LocalNode::start(&owner_address.to_bech32(NETWORK), GENESIS_SPARKS).await;
    let chain = Chain::connect(&node.url()).expect("a client");
    assert_eq!(chain.tip_height().await.expect("the tip"), 0);

    // The owner finds the genesis allocation: in the clear, so with no opening.
    let synced = wallet::sync(&chain, &parties.owner, 0..1, 0..1)
        .await
        .expect("sync the owner");
    assert!(synced.skipped.is_empty());
    assert_eq!(synced.outputs.len(), 1);
    let genesis = &synced.outputs[0];
    assert_eq!(genesis.qty, GENESIS_SPARKS);
    assert!(genesis.opening.is_none() && genesis.spent.is_none());
    assert_eq!(
        (genesis.branch, genesis.index, genesis.height),
        (util::RECEIVING, 0, 0)
    );

    // The owner funds one voucher per denomination, with the rest as change.
    let policies: Vec<VoucherPolicy> = DENOMINATIONS
        .iter()
        .map(|_| parties.policy(&mut rng))
        .collect();
    let prepared: Vec<PreparedOutput> = DENOMINATIONS
        .iter()
        .map(|qty| {
            let spec = OutputSpec {
                memo: MEMO.to_vec(),
                ..output(merchant, *qty)
            };
            prepare_output(&spec, &mut rng).expect("prepare a voucher")
        })
        .collect();
    let allowance: u64 = DENOMINATIONS.iter().sum();
    let change_address = parties
        .owner
        .address_at(util::CHANGE, 0)
        .expect("change address");
    let change = prepare_output(
        &output(change_address, GENESIS_SPARKS - allowance),
        &mut rng,
    )
    .expect("prepare the change");
    let mut payouts: Vec<Payout<'_>> = policies
        .iter()
        .zip(&prepared)
        .map(|(policy, prepared)| Payout::Voucher { policy, prepared })
        .collect();
    payouts.push(Payout::Wallet {
        to: change_address.spending_key().compress(),
        prepared: &change,
    });

    let proofs = chain
        .fresh_proofs(&[genesis.id])
        .await
        .expect("a proof of the genesis allocation");
    let input = genesis
        .to_input(&parties.owner, proofs[0].clone())
        .expect("the genesis allocation as an input");
    let unsigned = build::funding(std::slice::from_ref(&input), &payouts, 0).expect("funding");
    let created = build::outputs(unsigned.log());
    let vouchers: Vec<Voucher> = policies
        .iter()
        .zip(DENOMINATIONS)
        .map(|(policy, qty)| {
            let predicate = policy.predicate().expect("a predicate");
            let (contract, _) = created
                .iter()
                .find(|(contract, _)| contract.predicate.to_point() == predicate)
                .expect("the funding creates the voucher");
            Voucher::new(*policy, contract.clone(), qty).expect("a voucher")
        })
        .collect();
    let ids: Vec<[u8; 32]> = vouchers.iter().map(Voucher::id).collect();

    // The funding transaction's id is known before anything is signed or sent, so an owner can
    // save it with the openings first.
    let expected_txid = unsigned.log().txid();
    let tx = build::sign(unsigned, &[input.signing_key()]).expect("sign the funding");
    let bytes = build::package(tx, vec![input.proof().clone()]).expect("package the funding");

    // Before the funding, the vouchers do not exist for the node.
    for state in chain.states(&ids).await.expect("states") {
        assert!(matches!(state, ContractState::Unknown));
    }
    let funding = chain
        .submit(bytes)
        .await
        .expect("the node admits the funding");
    assert_eq!(funding, expected_txid);
    assert_eq!(
        chain.tx_state(&funding).await.expect("state"),
        TxState::Mempool
    );
    // A funded voucher still unknown to the chain is how an owner tells a pending funding apart.
    assert!(matches!(
        chain.states(&ids[..1]).await.expect("states")[0],
        ContractState::Unknown
    ));
    let height = confirm(&node, &chain, &funding).await;
    assert_eq!(height, 1);
    assert_eq!(
        chain.tx_state(&funding).await.expect("state"),
        TxState::Confirmed { height: 1 }
    );

    // The node publishes exactly the contracts the owner derived before submitting, each still
    // holding the receipt that was prepared for the merchant.
    for (index, (voucher, policy)) in vouchers.iter().zip(&policies).enumerate() {
        let published = chain
            .contract(&voucher.id())
            .await
            .expect("the contract")
            .expect("the node archived the voucher");
        let published =
            Voucher::new(*policy, published, DENOMINATIONS[index]).expect("a published voucher");
        assert_eq!(published.id(), voucher.id());
        assert_eq!(
            published.receipt().expect("a receipt"),
            prepared[index].note
        );
    }
    assert!(
        chain
            .contract(&[0xcc; 32])
            .await
            .expect("an answer")
            .is_none()
    );

    let states = chain.states(&ids).await.expect("states");
    assert!(
        states
            .iter()
            .all(|state| matches!(state, ContractState::Unspent(_)))
    );
    let ContractState::Spent { height, txid } =
        chain.states(&[genesis.id]).await.expect("states")[0]
    else {
        panic!("the genesis allocation is spent");
    };
    assert_eq!((height, txid), (1, funding));

    // The owner's wallet now holds the change, opened from its note, and the genesis is spent.
    let synced = wallet::sync(&chain, &parties.owner, 0..1, 0..1)
        .await
        .expect("sync the owner");
    let unspent: Vec<_> = synced
        .outputs
        .iter()
        .filter(|o| o.spent.is_none())
        .collect();
    assert_eq!(unspent.len(), 1);
    assert_eq!(unspent[0].qty, GENESIS_SPARKS - allowance);
    assert!(unspent[0].opening.is_some());
    assert_eq!(unspent[0].txid, funding);
    assert_eq!(synced.outputs[0].spent, Some((1, funding)));

    // The delegate redeems the 50 and the 10 with proofs taken just before. A proof of one of
    // the vouchers it leaves alone is kept back, to see what the redemption's block does to it.
    let held = chain
        .fresh_proofs(&[ids[1]])
        .await
        .expect("a proof of the first 20");
    let paying = [&vouchers[0], &vouchers[3]];
    let paying_ids = [ids[0], ids[3]];
    let proofs = chain.fresh_proofs(&paying_ids).await.expect("proofs");
    let redemption = redeem(&chain, &parties, &paying, proofs)
        .await
        .expect("the node admits the redemption");
    assert_eq!(confirm(&node, &chain, &redemption).await, 2);

    let states = chain.states(&ids).await.expect("states");
    for (index, state) in states.iter().enumerate() {
        match (index, state) {
            (0 | 3, ContractState::Spent { height: 2, txid }) => assert_eq!(*txid, redemption),
            (1 | 2, ContractState::Unspent(_)) => {}
            (index, other) => panic!("voucher {index} is {other}"),
        }
    }

    // One block is enough to make a proof stale: the proof held back since before the redemption's
    // block no longer proves anything, and the node refuses a redemption that carries it.
    let stale = redeem(&chain, &parties, &[&vouchers[1]], held)
        .await
        .expect_err("the node refuses a stale proof");
    assert!(
        matches!(stale, ChainError::Rejected { code, .. } if code == codes::MEMPOOL_REJECTED),
        "{stale}"
    );
    assert!(stale.is_stale_proof(), "{stale}");

    // The merchant finds both payments, with their receipts' notes, through a plain scan.
    let predicate = merchant.spending_key().compress().to_bytes();
    let hits = chain.scan(&[predicate], 0).await.expect("scan");
    assert_eq!(hits.len(), 2);
    let view_key = parties
        .merchant
        .viewing_key_at(util::RECEIVING, 0)
        .expect("view key");
    let mut amounts = Vec::new();
    for hit in &hits {
        assert_eq!((hit.height, hit.txid), (2, redemption));
        assert!(hit.spent.is_none());
        let note = hit.note.as_deref().expect("the receipt follows the payout");
        let opened =
            open_note(&hit.contract, Some(note), &merchant, &view_key).expect("the receipt opens");
        assert_eq!(opened.memo, MEMO);
        amounts.push(opened.opening.qty);
    }
    amounts.sort_unstable();
    assert_eq!(amounts, [10, 50]);
    // Scanning from a later height finds nothing, and the node's order is by height.
    assert!(chain.scan(&[predicate], 3).await.expect("scan").is_empty());

    // Lists longer than the node answers at once are split, and the answers stay in order: the
    // real contracts sit in the second batch of ids and the third batch of predicates.
    let too_many = |n: u32| {
        let mut id = [0u8; 32];
        id[..4].copy_from_slice(&n.to_le_bytes());
        id
    };
    let mut many: Vec<[u8; 32]> = (0..2_500).map(too_many).collect();
    many[1_500] = ids[0];
    many[2_499] = ids[1];
    let states = chain
        .states(&many)
        .await
        .expect("states of 2,500 contracts");
    assert_eq!(states.len(), 2_500);
    for (position, state) in states.iter().enumerate() {
        match (position, state) {
            (1_500, ContractState::Spent { txid, .. }) => assert_eq!(*txid, redemption),
            (2_499, ContractState::Unspent(_)) => {}
            (_, ContractState::Unknown) => {}
            (position, other) => panic!("contract {position} is {other}"),
        }
    }
    let mut predicates: Vec<[u8; 32]> = (0..1_100).map(|n| too_many(n + 10_000)).collect();
    predicates.push(predicate);
    let hits = chain
        .scan(&predicates, 0)
        .await
        .expect("scan 1,101 predicates");
    assert_eq!(hits.len(), 2);

    // The same through the wallet, which also keeps the openings it needs to spend them.
    let synced = wallet::sync(&chain, &parties.merchant, 0..1, 0..0)
        .await
        .expect("sync the merchant");
    assert!(synced.skipped.is_empty());
    assert_eq!(synced.outputs.len(), 2);
    assert_eq!(synced.outputs.iter().map(|o| o.qty).sum::<u64>(), 60);
    assert!(
        synced
            .outputs
            .iter()
            .all(|o| o.opening.is_some() && o.memo == MEMO)
    );

    // The merchant spends both to a fresh address, which shows the payments are usable money.
    let fresh = parties
        .merchant
        .address_at(util::RECEIVING, 1)
        .expect("fresh address");
    let received_ids: Vec<[u8; 32]> = synced.outputs.iter().map(|o| o.id).collect();
    let proofs = chain.fresh_proofs(&received_ids).await.expect("proofs");
    let inputs: Vec<_> = synced
        .outputs
        .iter()
        .zip(proofs)
        .map(|(output, proof)| output.to_input(&parties.merchant, proof).expect("an input"))
        .collect();
    let to_fresh = prepare_output(&output(fresh, 60), &mut rng).expect("prepare");
    let payout = [Payout::Wallet {
        to: fresh.spending_key().compress(),
        prepared: &to_fresh,
    }];
    let unsigned = build::funding(&inputs, &payout, 0).expect("the merchant's spend");
    let keys: Vec<_> = inputs.iter().map(|input| input.signing_key()).collect();
    let tx = build::sign(unsigned, &keys).expect("sign");
    let proofs = inputs.iter().map(|input| input.proof().clone()).collect();
    let spend = chain
        .submit(build::package(tx, proofs).expect("package"))
        .await
        .expect("the node admits the merchant's spend");
    // This one is mined while the merchant is already waiting, so the wait has to poll.
    let waiting = chain.wait_confirmed(&spend, PATIENCE);
    let minting = async {
        time::sleep(Duration::from_millis(400)).await;
        node.mint()
    };
    let (waited, minted) = tokio::join!(waiting, minting);
    assert_eq!((waited.expect("the spend confirms"), minted), (3, 3));

    let synced = wallet::sync(&chain, &parties.merchant, 0..2, 0..0)
        .await
        .expect("sync the merchant");
    assert_eq!(synced.outputs.len(), 3);
    for output in &synced.outputs {
        match output.index {
            0 => assert_eq!(output.spent, Some((3, spend))),
            1 => assert!(output.spent.is_none() && output.qty == 60),
            other => panic!("unexpected address index {other}"),
        }
    }

    // Fetching the proof again and rebuilding the transaction gets it accepted.
    let fresh_proofs = chain.fresh_proofs(&[ids[1]]).await.expect("a fresh proof");
    let accepted = redeem(&chain, &parties, &[&vouchers[1]], fresh_proofs.clone())
        .await
        .expect("the node admits the refreshed redemption");
    assert_eq!(confirm(&node, &chain, &accepted).await, 4);
    assert!(matches!(
        chain.states(&[ids[1]]).await.expect("states")[0],
        ContractState::Spent { height: 4, txid } if txid == accepted
    ));

    // A voucher that is spent is refused in the same words, and the refresh ends it for good.
    let again = redeem(&chain, &parties, &[&vouchers[1]], fresh_proofs).await;
    let error = again.expect_err("a spent voucher is refused");
    assert!(error.is_stale_proof(), "{error}");
    let error = chain
        .fresh_proofs(&[ids[1]])
        .await
        .expect_err("a spent voucher has no proof to refresh");
    assert!(
        matches!(error, ChainError::NotUnspent(id, _) if id == ids[1]),
        "{error}"
    );
    assert!(error.to_string().contains("spent at height 4"), "{error}");
    assert!(!error.is_stale_proof());

    // The other 20 stays unspent, and a transaction nobody sent never confirms.
    assert!(matches!(
        chain.states(&[ids[2]]).await.expect("states")[0],
        ContractState::Unspent(_)
    ));
    let nobody = TxID([1; 32]);
    assert_eq!(
        chain.tx_state(&nobody).await.expect("state"),
        TxState::Unknown
    );
    let waited = chain
        .wait_confirmed(&nobody, Duration::from_millis(600))
        .await;
    assert!(matches!(waited, Err(ChainError::Timeout)), "{waited:?}");

    // A node that is gone answers nothing, which a caller must read as an unknown outcome.
    let url = node.url();
    drop(chain);
    node.stop().await;
    let gone = Chain::connect(&url).expect("a client");
    let error = gone.tip_height().await.expect_err("the node is stopped");
    assert!(matches!(error, ChainError::Transport(_)), "{error}");
    assert!(error.to_string().contains("outcome is unknown"), "{error}");
}
