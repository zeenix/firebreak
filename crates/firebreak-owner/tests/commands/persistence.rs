//! What survives when funding goes wrong: the allowance is saved before the node is asked, and
//! the node's answer, or its silence, decides what is saved next.

use firebreak_core::ChainError;
use firebreak_core::chain::{ContractState, TxState};
use firebreak_core::journal::{Action, Actor, Outcome, Stage};
use firebreak_core::store::VoucherState;
use firebreak_owner::{Error, Reclaim, create_allowance, prepare, reclaim, status, submit};

use crate::harness::{GENESIS, Harness};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_allowance_is_saved_with_its_openings_before_anything_is_sent() {
    let harness = Harness::start(false).await;
    let prepared = prepare(&harness.ctx, &harness.request(&[50, 20, 20, 10], false))
        .await
        .expect("prepare");

    // The allowance is on disk in state prepared, with the opening of every voucher.
    let store = harness.files().load().expect("the store");
    assert_eq!(store.allowances.len(), 1);
    let allowance = &store.allowances[0];
    assert_eq!(allowance.allowance, prepared.allowance());
    assert_eq!(allowance.funding_txid, prepared.txid());
    let quantities: Vec<u64> = allowance.vouchers.iter().map(|v| v.qty).collect();
    assert_eq!(quantities, [50, 20, 20, 10]);
    for voucher in &allowance.vouchers {
        assert_eq!(voucher.state, VoucherState::Prepared);
        assert_eq!(voucher.opening.qty, voucher.qty);
    }
    assert_eq!(
        store.next_change_index, 1,
        "the change address was issued before it was used"
    );

    // The node has heard nothing yet, and nothing was written for anyone else.
    let ids: Vec<[u8; 32]> = allowance.vouchers.iter().map(|v| v.id).collect();
    for state in harness.ctx.chain.states(&ids).await.expect("states") {
        assert!(matches!(state, ContractState::Unknown));
    }
    let funding = harness
        .ctx
        .chain
        .tx_state(&prepared.txid())
        .await
        .expect("state");
    assert_eq!(funding, TxState::Unknown);
    assert!(!harness.files().package(&allowance.allowance).exists());
    assert!(!harness.files().journal().exists());
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_that_goes_silent_leaves_the_funding_pending_and_nothing_lost() {
    let harness = Harness::start(false).await;
    let prepared = prepare(&harness.ctx, &harness.request(&[50, 20, 20, 10], false))
        .await
        .expect("prepare");
    let (allowance, txid) = (prepared.allowance().to_owned(), prepared.txid());

    // The node cannot be reached when the funding is offered.
    let error = submit(&harness.dead(), prepared)
        .await
        .expect_err("nobody answers");
    assert!(matches!(error, Error::Unknown { .. }), "{error}");
    let shown = error.to_string();
    assert!(shown.starts_with("unknown:"), "{shown}");
    assert!(shown.contains("outcome is unknown"), "{shown}");
    assert!(shown.contains("firebreak-owner status"), "{shown}");
    assert!(shown.contains("funding_pending"), "{shown}");

    // Everything the owner needs is still on disk, and the vouchers wait on the funding.
    let store = harness.files().load().expect("the store");
    let record = store.allowance(&allowance).expect("the allowance");
    assert_eq!(record.funding_txid, txid);
    assert_eq!(record.vouchers.len(), 4);
    for voucher in &record.vouchers {
        assert_eq!(voucher.state, VoucherState::FundingPending);
        assert_eq!(voucher.opening.qty, voucher.qty);
    }
    // The package is written, since the node may have the transaction.
    assert!(harness.files().package(&allowance).exists());

    // The attempt is journaled with the bytes that were sent and the transport's own words.
    let journal = harness.journal();
    assert_eq!(journal.len(), 1);
    let entry = &journal[0];
    assert_eq!(
        (entry.actor, entry.action.clone()),
        (Actor::Owner, Action::Fund)
    );
    assert_eq!(
        (entry.stage, entry.outcome),
        (Stage::Node, Outcome::Unknown)
    );
    assert_eq!(entry.txid, Some(txid));
    assert!(
        entry
            .error
            .as_deref()
            .is_some_and(|text| text.contains("outcome is unknown"))
    );
    let bytes = entry.tx.clone().expect("the bytes that were sent");

    // Suppose the node had the transaction after all: only the answer was lost. The bytes in the
    // journal are exactly what it took.
    let accepted = harness
        .ctx
        .chain
        .submit(bytes)
        .await
        .expect("the node takes the bytes");
    assert_eq!(accepted, txid);
    harness.mint().await;

    let seen = status(&harness.ctx).await.expect("status finds out");
    let record = seen
        .status
        .allowances
        .iter()
        .find(|a| a.allowance == allowance)
        .expect("listed");
    assert!(
        record
            .vouchers
            .iter()
            .all(|v| v.state == VoucherState::Unspent)
    );
    assert_eq!(seen.status.wallet.balance, 900);

    // Nothing is lost: the owner takes every voucher back with what it saved.
    let reclaimed = reclaim(&harness.ctx, &Reclaim::default())
        .await
        .expect("reclaim");
    assert_eq!(reclaimed.recoveries.len(), 1);
    harness.mint().await;
    let done = status(&harness.ctx).await.expect("status");
    assert_eq!(done.status.wallet.balance, GENESIS);
    assert!(
        done.status.allowances[0]
            .vouchers
            .iter()
            .all(|v| v.state == VoucherState::Recovered)
    );
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_funding_the_node_never_saw_goes_back_to_prepared() {
    let harness = Harness::start(false).await;
    let prepared = prepare(&harness.ctx, &harness.request(&[50, 20, 20, 10], false))
        .await
        .expect("prepare");
    let allowance = prepared.allowance().to_owned();
    submit(&harness.dead(), prepared)
        .await
        .expect_err("nobody answers");

    // The request never arrived: the node does not know the funding, so status says the vouchers
    // were never funded, and the wallet is whole.
    let seen = status(&harness.ctx).await.expect("status finds out");
    assert_eq!(seen.status.wallet.balance, GENESIS);
    let record = seen
        .status
        .allowances
        .iter()
        .find(|a| a.allowance == allowance)
        .expect("listed");
    assert!(
        record
            .vouchers
            .iter()
            .all(|v| v.state == VoucherState::Prepared)
    );

    // Recovery leaves them alone and says why.
    let reclaimed = reclaim(&harness.ctx, &Reclaim::default())
        .await
        .expect("reclaim");
    assert!(reclaimed.recoveries.is_empty());
    assert_eq!(reclaimed.skipped.len(), 4);
    assert!(
        reclaimed
            .skipped
            .iter()
            .all(|skip| skip.reason.contains("never funded"))
    );
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_funding_leaves_the_allowance_prepared_and_writes_no_package() {
    let harness = Harness::start(true).await;
    let ctx = &harness.ctx;

    // Two allowances are prepared from the same wallet output, and one is funded first.
    let refused = prepare(ctx, &harness.request(&[50, 20, 20, 10], false))
        .await
        .expect("prepare");
    let allowance = refused.allowance().to_owned();
    let funded = create_allowance(ctx, &harness.request(&[30], true))
        .await
        .expect("fund the other allowance");

    let error = submit(ctx, refused).await.expect_err("the output is spent");
    assert!(
        matches!(error, Error::Node(ChainError::NotUnspent(..))),
        "{error}"
    );
    let shown = error.to_string();
    assert!(shown.starts_with("node:"), "{shown}");
    assert!(shown.contains("is not unspent"), "{shown}");

    // The refused allowance stays prepared, and nothing was written for a delegate.
    assert!(!harness.files().package(&allowance).exists());
    let snapshot = harness.snapshot();
    let state_of = |id: &str| {
        let record = snapshot
            .allowances
            .iter()
            .find(|a| a.allowance == id)
            .expect("listed");
        record.vouchers.iter().map(|v| v.state).collect::<Vec<_>>()
    };
    assert_eq!(state_of(&allowance), [VoucherState::Prepared; 4]);
    assert_eq!(state_of(&funded.allowance), [VoucherState::Unspent]);

    // The journal has the node's refusal word for word, after the funding that won.
    let journal = harness.journal();
    assert_eq!(journal.len(), 2);
    assert_eq!(journal[0].outcome, Outcome::Accepted);
    assert_eq!(journal[1].outcome, Outcome::Rejected);
    assert!(
        journal[1]
            .error
            .as_deref()
            .is_some_and(|text| text.contains("refused"))
    );
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_proof_is_refreshed_once_and_the_same_transaction_is_accepted() {
    let harness = Harness::start(true).await;
    let ctx = &harness.ctx;
    let first = create_allowance(ctx, &harness.request(&[50, 20], true))
        .await
        .expect("fund the first allowance");

    // The second allowance is prepared around proofs that are fresh now, and then a block that
    // holds a transaction makes them stale: the delegate redeems a voucher of the first.
    let second = prepare(ctx, &harness.request(&[40, 10], false))
        .await
        .expect("prepare");
    let (allowance, txid) = (second.allowance().to_owned(), second.txid());
    let vouchers = harness.vouchers(&first.allowance);
    let redemption = harness.redeem(&[&vouchers[0]]).await;
    harness.confirmed(&redemption).await;
    let saved: Vec<[u8; 32]> = harness
        .files()
        .load()
        .expect("the store")
        .allowance(&allowance)
        .expect("saved")
        .vouchers
        .iter()
        .map(|v| v.id)
        .collect();

    let report = submit(ctx, second)
        .await
        .expect("the refreshed transaction is accepted");
    assert_eq!(report.funding_txid, txid);
    assert_eq!(
        report.vouchers.iter().map(|v| v.id).collect::<Vec<_>>(),
        saved
    );
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);

    // Both attempts are journaled, with the same transaction: the node's refusal, then its
    // acceptance.
    let journal = harness.journal();
    let attempts: Vec<_> = journal
        .iter()
        .filter(|e| e.action == Action::Fund)
        .skip(1)
        .collect();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].outcome, Outcome::Rejected);
    let refusal = attempts[0].error.as_deref().expect("the node's refusal");
    assert!(
        refusal.contains("refused") && refusal.contains("proof"),
        "{refusal}"
    );
    assert_eq!(attempts[1].outcome, Outcome::Accepted);
    assert!(attempts.iter().all(|entry| entry.txid == Some(txid)));
    assert_ne!(
        attempts[0].tx, attempts[1].tx,
        "the second attempt carries the fresh proofs"
    );

    // The vouchers on disk are the vouchers the node has once the transaction is in a block.
    harness.confirmed(&txid).await;
    let seen = status(ctx).await.expect("status");
    let record = seen
        .status
        .allowances
        .iter()
        .find(|a| a.allowance == allowance)
        .expect("listed");
    assert_eq!(
        record.vouchers.iter().map(|v| v.id).collect::<Vec<_>>(),
        saved
    );
    assert!(
        record
            .vouchers
            .iter()
            .all(|v| v.state == VoucherState::Unspent)
    );
    harness.finish().await;
}
