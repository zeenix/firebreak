//! A node or a network that goes wrong around a payment.

use std::time::Duration;

use firebreak_agent::Agent;
use firebreak_agent::pay::Stage;
use firebreak_core::build;
use firebreak_core::chain::Chain;
use firebreak_core::devnet::output;
use firebreak_core::journal::{self, JournalEntry, Outcome};
use firebreak_core::store::VoucherState;
use flamekd::util;
use flamepayments::prepare_output;

use crate::fixture::World;
use crate::proxy::{Fault, Proxy, STALE};

const WAIT: Duration = Duration::from_secs(30);

/// An agent on the files of `world` that reaches the node through `proxy`.
fn through(world: &World, proxy: &Proxy) -> Agent {
    let chain = Chain::connect(proxy.url()).expect("a client");
    Agent::new(world.files.clone(), chain)
}

/// What the journal holds.
fn journaled(world: &World) -> Vec<JournalEntry> {
    let journal = journal::read_all(&world.files.journal()).expect("the journal");
    assert_eq!(journal.skipped, 0);
    journal.entries
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_proof_gets_fresh_proofs_and_a_rebuilt_transaction_once() {
    let world = World::start().await;
    let proxy = Proxy::start(&world.net.node.url()).await;
    proxy.inject(Fault::Refuse(STALE.to_owned()));
    let agent = through(&world, &proxy);

    let payment = agent
        .pay(&world.request(60), Some(WAIT))
        .await
        .expect("the payment");
    assert_eq!(payment.state, VoucherState::Redeemed);

    // Between the refusal and the second submission the agent asked for proofs again.
    let calls = proxy.calls();
    let first = calls
        .iter()
        .position(|call| call == "submit_tx")
        .expect("a submission");
    assert_eq!(
        calls[first..first + 3],
        ["submit_tx", "proofs", "submit_tx"]
    );

    // Both attempts are journaled: the refusal in the node's words, and the acceptance.
    let entries = journaled(&world);
    let [refused, accepted] = entries.as_slice() else {
        panic!("expected two entries, got {}", entries.len());
    };
    assert_eq!(refused.outcome, Outcome::Rejected);
    assert_eq!(refused.stage, journal::Stage::Node);
    let error = refused.error.as_deref().expect("the node's error");
    assert!(error.contains(STALE), "{error}");
    assert!(error.contains("-32002"), "{error}");
    assert!(refused.tx.is_some());
    assert_eq!(accepted.outcome, Outcome::Accepted);
    assert_eq!(refused.txid, accepted.txid);
    assert_eq!(accepted.txid, Some(payment.txid));
    assert_eq!(refused.inputs, accepted.inputs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_stale_proof_ends_the_payment_and_frees_the_vouchers() {
    let world = World::start().await;
    let proxy = Proxy::start(&world.net.node.url()).await;
    proxy.inject(Fault::Refuse(STALE.to_owned()));
    proxy.inject(Fault::Refuse(STALE.to_owned()));
    let agent = through(&world, &proxy);

    let error = agent
        .pay(&world.request(60), Some(WAIT))
        .await
        .expect_err("refused twice");
    assert_eq!(error.stage, Stage::Node);
    assert!(error.message.contains(STALE), "{}", error.message);
    assert!(error.txid.is_some());

    // The transaction was rebuilt once, not endlessly, and every attempt is journaled.
    let submissions = proxy
        .calls()
        .iter()
        .filter(|call| *call == "submit_tx")
        .count();
    assert_eq!(submissions, 2);
    let entries = journaled(&world);
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|entry| entry.outcome == Outcome::Rejected)
    );

    // The node holds nothing of it, so reconciling freed the vouchers, and paying works again.
    let freed = world.recorded().await;
    assert!(
        freed
            .iter()
            .all(|(_, state, txid)| *state == VoucherState::Unspent && txid.is_none())
    );
    let payment = world
        .agent
        .pay(&world.request(60), Some(WAIT))
        .await
        .expect("the payment");
    assert_eq!(payment.state, VoucherState::Redeemed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refusal_is_the_nodes_word_and_gives_the_vouchers_back() {
    let world = World::start().await;
    let proxy = Proxy::start(&world.net.node.url()).await;
    proxy.inject(Fault::Refuse("mempool policy limit reached".to_owned()));
    let agent = through(&world, &proxy);

    let error = agent
        .pay(&world.request(60), Some(WAIT))
        .await
        .expect_err("refused");
    assert_eq!(error.stage, Stage::Node);
    assert_eq!(
        error.message,
        "the node refused the transaction (code -32002): mempool policy limit reached"
    );
    // Only a stale proof is tried again.
    let submissions = proxy
        .calls()
        .iter()
        .filter(|call| *call == "submit_tx")
        .count();
    assert_eq!(submissions, 1);
    let entries = journaled(&world);
    let [entry] = entries.as_slice() else {
        panic!("expected one entry, got {}", entries.len());
    };
    assert_eq!(entry.outcome, Outcome::Rejected);
    assert_eq!(entry.error.as_deref(), Some(error.message.as_str()));
    let freed = world.recorded().await;
    assert!(
        freed
            .iter()
            .all(|(_, state, txid)| *state == VoucherState::Unspent && txid.is_none())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_answer_leaves_the_outcome_unknown_and_the_next_reconciliation_decides() {
    let world = World::start_manual().await;
    let proxy = Proxy::start(&world.net.node.url()).await;
    proxy.inject(Fault::LoseAnswer);
    let agent = through(&world, &proxy);

    let error = agent
        .pay(&world.request(60), Some(WAIT))
        .await
        .expect_err("no answer");
    assert_eq!(error.stage, Stage::Unknown);
    assert!(
        error.message.contains("outcome is unknown"),
        "{}",
        error.message
    );
    assert!(
        error.message.contains("may have reached the node"),
        "{}",
        error.message
    );
    let txid = error
        .txid
        .expect("the transaction that may have been taken");

    // The vouchers stay reserved, and the journal says that the outcome is unknown.
    let txid = Some(txid);
    let reserved = [
        (50, VoucherState::RedemptionPending, txid),
        (20, VoucherState::Unspent, None),
        (20, VoucherState::Unspent, None),
        (10, VoucherState::RedemptionPending, txid),
    ];
    assert_eq!(world.recorded().await, reserved);
    let entries = journaled(&world);
    let [entry] = entries.as_slice() else {
        panic!("expected one entry, got {}", entries.len());
    };
    assert_eq!(entry.outcome, Outcome::Unknown);
    assert_eq!(entry.txid, txid);
    assert!(entry.tx.is_some());

    // The node did take the transaction. Asked again, it has it in its mempool, and then in a
    // block, and the vouchers are redeemed.
    world.agent.status().await.expect("a status");
    assert_eq!(world.recorded().await, reserved);
    world.net.node.mint();
    world.agent.status().await.expect("a status");
    let redeemed = [
        (50, VoucherState::Redeemed, txid),
        (20, VoucherState::Unspent, None),
        (20, VoucherState::Unspent, None),
        (10, VoucherState::Redeemed, txid),
    ];
    assert_eq!(world.recorded().await, redeemed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_request_is_found_out_by_the_next_reconciliation_and_the_payment_can_be_made_again()
{
    let world = World::start_manual().await;
    let proxy = Proxy::start(&world.net.node.url()).await;
    proxy.inject(Fault::LoseRequest);
    let agent = through(&world, &proxy);

    let error = agent
        .pay(&world.request(60), None)
        .await
        .expect_err("no answer");
    assert_eq!(error.stage, Stage::Unknown);
    let lost = error
        .txid
        .expect("the transaction that may have been taken");
    let recorded = world.recorded().await;
    assert_eq!(
        recorded[0],
        (50, VoucherState::RedemptionPending, Some(lost))
    );

    // The node never saw it, so the reservation was only a belief, and the next reconciliation
    // drops it.
    world.agent.status().await.expect("a status");
    let freed = world.recorded().await;
    assert!(
        freed
            .iter()
            .all(|(_, state, txid)| *state == VoucherState::Unspent && txid.is_none())
    );

    // The same vouchers pay again, and make the same transaction.
    let payment = world
        .agent
        .pay(&world.request(60), None)
        .await
        .expect("the payment");
    assert_eq!(payment.txid, lost);
    assert_eq!(payment.state, VoucherState::RedemptionPending);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_voucher_the_owner_recovered_meanwhile_is_reported_and_reconciled() {
    let mut world = World::start().await;
    let proxy = Proxy::start(&world.net.node.url()).await;

    // The agent pays 10 with the one voucher of 10. Just as it submits, the owner recovers that
    // voucher with a block of its own, so the node refuses the proofs, and the fresh ones that
    // the agent asks for then show that the voucher is spent.
    let ten = world.funded.vouchers[3].clone();
    let opening = world.funded.openings[3];
    let back = world
        .net
        .parties
        .owner
        .address_at(util::CHANGE, 7)
        .expect("a recovery address");
    let prepared = prepare_output(&output(back, 10), &mut world.net.rng).expect("prepare");
    let unsigned = build::recovery(
        &[(&ten, &opening)],
        back.spending_key().compress(),
        &prepared,
        0,
    )
    .expect("build the recovery");
    let tx = build::sign(unsigned, &[world.net.parties.owner_key]).expect("sign the recovery");
    let chain = world.net.chain.clone();
    let node = std::sync::Arc::clone(&world.net.node);
    let id = ten.id();
    let recover: crate::proxy::Effect = Box::new(move || {
        Box::pin(async move {
            let proofs = chain
                .fresh_proofs(&[id])
                .await
                .expect("a proof of the voucher");
            let bytes = build::package(tx, proofs).expect("package the recovery");
            chain
                .submit(bytes)
                .await
                .expect("the node admits the recovery");
            node.mint();
        })
    });
    proxy.inject(Fault::RefuseAfter(STALE.to_owned(), recover));
    let agent = through(&world, &proxy);

    let error = agent
        .pay(&world.request(10), Some(WAIT))
        .await
        .expect_err("the voucher is gone");
    assert_eq!(error.stage, Stage::Node);
    assert!(
        error.message.contains("is not unspent"),
        "{}",
        error.message
    );
    assert!(
        error.message.contains("spent at height"),
        "{}",
        error.message
    );
    assert!(error.message.contains("not submitted"), "{}", error.message);

    // The voucher is recovered and the others are as they were. Nothing stays reserved.
    let recorded = world.recorded().await;
    let states: Vec<_> = recorded
        .iter()
        .map(|(qty, state, txid)| (*qty, *state, *txid))
        .collect();
    assert_eq!(
        states,
        [
            (50, VoucherState::Unspent, None),
            (20, VoucherState::Unspent, None),
            (20, VoucherState::Unspent, None),
            (10, VoucherState::Recovered, None),
        ]
    );
    // Only the first attempt, which the node refused, was submitted and journaled.
    let entries = journaled(&world);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].outcome, Outcome::Rejected);

    // What is left cannot make 10 any more.
    let error = world
        .agent
        .pay(&world.request(10), None)
        .await
        .expect_err("no 10");
    assert_eq!(
        error.message,
        "no exact combination of vouchers for 10 (denominations: 50, 20, 20)"
    );
}
