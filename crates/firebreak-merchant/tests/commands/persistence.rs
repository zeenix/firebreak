//! What survives when spending goes wrong: the spend is saved before the node is asked, and the
//! node's answer, or its silence, decides what is saved next.

use firebreak_core::ChainError;
use firebreak_core::chain::TxState;
use firebreak_core::journal::{Action, Actor, Outcome, Stage};
use firebreak_core::store::{MerchantStore, Progress};
use firebreak_merchant::{Error, inspect, prepare, spend, submit};
use flamekd::util;

use crate::harness::Harness;

/// A harness whose merchant has been paid 50 and 10, in a block.
async fn paid(mining: bool) -> Harness {
    let harness = Harness::start(mining).await;
    let funded = harness.fund(&[50, 20, 20, 10]).await;
    let vouchers = harness.vouchers(&funded.allowance);
    harness.pay(&[&vouchers[0], &vouchers[3]]).await;
    harness
}

fn store(harness: &Harness) -> MerchantStore {
    MerchantStore::load(&harness.files().store()).expect("the store")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_spend_is_saved_before_it_is_sent_and_a_silent_node_leaves_it_pending() {
    let harness = paid(false).await;
    let prepared = prepare(&harness.ctx)
        .await
        .expect("prepare")
        .expect("payments to spend");
    let txid = prepared.txid();

    // The spend is on disk, pending, with the fresh address it pays issued, before the node has
    // been asked anything.
    let saved = store(&harness);
    let fresh = saved
        .account()
        .expect("an account")
        .address_at(util::RECEIVING, 1)
        .expect("an address");
    assert_eq!(saved.next_index, 2);
    assert_eq!(saved.spends.len(), 1);
    assert_eq!(saved.spends[0].txid, txid);
    assert_eq!((saved.spends[0].qty, saved.spends[0].to), (60, fresh));
    assert_eq!(saved.spends[0].state, Progress::Pending);
    assert_eq!(
        harness.ctx.chain.tx_state(&txid).await.expect("state"),
        TxState::Unknown,
        "the node has heard nothing"
    );
    let journal_before = harness.journal();
    assert_eq!(journal_before.len(), 0);

    // The node cannot be reached when the spend is offered.
    let error = submit(&harness.dead(), prepared, false)
        .await
        .expect_err("nobody answers");
    assert!(matches!(error, Error::Unknown { .. }), "{error}");
    let shown = error.to_string();
    assert!(shown.starts_with("unknown:"), "{shown}");
    assert!(shown.contains("outcome is unknown"), "{shown}");
    assert!(shown.contains("firebreak-merchant inspect"), "{shown}");
    let saved = store(&harness);
    assert_eq!(saved.spends.len(), 1);
    assert_eq!(saved.spends[0].state, Progress::Pending);

    // The attempt is journaled with the bytes that were sent and the transport's own words.
    let journal = harness.journal();
    assert_eq!(journal.len(), 1);
    let entry = &journal[0];
    assert_eq!(
        (entry.actor, entry.action.clone()),
        (Actor::Merchant, Action::Spend)
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

    // Suppose the node had the spend after all: only the answer was lost.
    let accepted = harness
        .ctx
        .chain
        .submit(bytes)
        .await
        .expect("the node takes the bytes");
    assert_eq!(accepted, txid);
    harness.mint().await;
    let seen = inspect(&harness.ctx).await.expect("inspect finds out");
    assert_eq!(seen.status.spends.len(), 1);
    assert_eq!(seen.status.spends[0].state, Progress::Confirmed);
    assert!(seen.status.receipts.iter().all(|receipt| receipt.spent));
    assert_eq!(
        seen.status.balance, 60,
        "nothing is lost: the 60 is at the fresh address"
    );
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_spend_the_node_never_saw_is_forgotten_and_can_be_made_again() {
    let harness = paid(true).await;
    let prepared = prepare(&harness.ctx)
        .await
        .expect("prepare")
        .expect("payments to spend");
    submit(&harness.dead(), prepared, false)
        .await
        .expect_err("nobody answers");

    // The request never arrived: the node does not know the spend, so it is forgotten, and the
    // payments are unspent.
    let seen = inspect(&harness.ctx).await.expect("inspect finds out");
    assert!(seen.status.spends.is_empty());
    assert!(seen.status.receipts.iter().all(|receipt| !receipt.spent));
    assert_eq!(seen.status.balance, 60);
    assert!(store(&harness).spends.is_empty());

    // Spending again works, and pays the next fresh address: the first was issued and not used.
    let spent = spend(&harness.ctx, true)
        .await
        .expect("spend")
        .spent
        .expect("a spend");
    let third = store(&harness)
        .account()
        .expect("an account")
        .address_at(util::RECEIVING, 2)
        .expect("an address");
    assert_eq!(spent.to, third);
    assert_eq!(
        inspect(&harness.ctx).await.expect("inspect").status.balance,
        60
    );
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pending_spend_refuses_another_until_it_confirms() {
    let harness = paid(false).await;
    let report = spend(&harness.ctx, false).await.expect("spend");
    let spent = report.spent.as_ref().expect("a spend");
    assert_eq!(spent.confirmed_height, None);
    assert!(report.to_string().contains("not confirmed yet"));
    let seen = inspect(&harness.ctx).await.expect("inspect");
    assert_eq!(seen.status.spends[0].state, Progress::Pending);
    assert!(seen.to_string().contains("pending"));

    // Spending the same payments again would conflict, so it is refused before anything is sent.
    let error = spend(&harness.ctx, false)
        .await
        .expect_err("a spend is pending");
    assert!(matches!(error, Error::Refused(_)), "{error}");
    assert!(
        error.to_string().contains(&hex::encode(spent.txid.0)),
        "{error}"
    );
    assert_eq!(harness.journal().len(), 1);

    // Once a block holds it, there is nothing left to spend.
    harness.mint().await;
    assert!(
        spend(&harness.ctx, false)
            .await
            .expect("spend")
            .spent
            .is_none()
    );
    let seen = inspect(&harness.ctx).await.expect("inspect");
    assert_eq!(seen.status.spends[0].state, Progress::Confirmed);
    assert_eq!(seen.status.balance, 60);
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_proof_is_refreshed_once_and_the_same_spend_goes_through() {
    let harness = Harness::start(true).await;
    let funded = harness.fund(&[50, 20, 20, 10]).await;
    let vouchers = harness.vouchers(&funded.allowance);
    harness.pay(&[&vouchers[0], &vouchers[3]]).await;

    // The spend is built around proofs that are fresh now, and then a block that holds a
    // transaction makes them stale: the delegate redeems another voucher.
    let prepared = prepare(&harness.ctx)
        .await
        .expect("prepare")
        .expect("payments to spend");
    let txid = prepared.txid();
    harness.pay(&[&vouchers[1]]).await;

    let report = submit(&harness.ctx, prepared, true)
        .await
        .expect("the refreshed spend is accepted");
    let spent = report.spent.expect("a spend");
    assert_eq!((spent.txid, spent.qty, spent.receipts), (txid, 60, 2));
    assert!(spent.confirmed_height.is_some());

    // Both attempts are journaled, with the same transaction: the node's refusal, then its
    // acceptance.
    let attempts = harness.journal();
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
        "the second attempt carries fresh proofs"
    );

    // 60 was spent to the fresh address, and the later payment of 20 is still there to spend.
    let seen = inspect(&harness.ctx).await.expect("inspect");
    assert_eq!(seen.status.balance, 80);
    assert_eq!(seen.status.spends[0].state, Progress::Confirmed);
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_spend_forgotten_by_an_inspection_in_between_is_saved_again() {
    let harness = paid(false).await;
    let prepared = prepare(&harness.ctx)
        .await
        .expect("prepare")
        .expect("payments to spend");
    let txid = prepared.txid();

    // An inspection that runs before the node has heard of the spend finds it unknown, and
    // forgets it.
    inspect(&harness.ctx).await.expect("inspect");
    assert!(store(&harness).spends.is_empty());

    // Offering it saves it again, and a block confirms it.
    submit(&harness.ctx, prepared, false).await.expect("submit");
    let saved = store(&harness);
    assert_eq!(saved.spends.len(), 1);
    assert_eq!(
        (saved.spends[0].txid, saved.spends[0].state),
        (txid, Progress::Pending)
    );
    harness.mint().await;
    let seen = inspect(&harness.ctx).await.expect("inspect");
    assert_eq!(seen.status.spends[0].state, Progress::Confirmed);
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_spend_is_forgotten_and_journaled() {
    let harness = paid(true).await;
    let refused = prepare(&harness.ctx)
        .await
        .expect("prepare")
        .expect("payments to spend");

    // Meanwhile the same payments are spent by another spend: an inspection forgets the first,
    // which the node has not heard of, and the second is made and confirmed.
    inspect(&harness.ctx).await.expect("inspect");
    let spent = spend(&harness.ctx, true)
        .await
        .expect("the other spend")
        .spent
        .expect("a spend");

    let error = submit(&harness.ctx, refused, false)
        .await
        .expect_err("the payments are spent");
    assert!(
        matches!(error, Error::Node(ChainError::NotUnspent(..))),
        "{error}"
    );
    assert!(error.to_string().starts_with("node:"), "{error}");

    // Only the spend that happened is on record, and the node's refusal is in the journal.
    let seen = inspect(&harness.ctx).await.expect("inspect");
    assert_eq!(seen.status.spends.len(), 1);
    assert_eq!(seen.status.spends[0].txid, spent.txid);
    assert_eq!(seen.status.balance, 60);
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
async fn inspecting_without_a_node_is_unknown_and_changes_nothing() {
    let harness = paid(true).await;
    inspect(&harness.ctx).await.expect("inspect");
    let snapshot = std::fs::read(harness.files().status()).expect("the snapshot");
    let store = std::fs::read(harness.files().store()).expect("the store");

    let error = inspect(&harness.dead()).await.expect_err("nobody answers");
    assert!(matches!(error, Error::Unknown { .. }), "{error}");
    let shown = error.to_string();
    assert!(
        shown.starts_with("unknown:") && shown.contains("outcome is unknown"),
        "{shown}"
    );
    assert_eq!(
        std::fs::read(harness.files().status()).expect("the snapshot"),
        snapshot
    );
    assert_eq!(
        std::fs::read(harness.files().store()).expect("the store"),
        store
    );

    let error = spend(&harness.dead(), false)
        .await
        .expect_err("nobody answers");
    assert!(matches!(error, Error::Unknown { .. }), "{error}");
    assert_eq!(
        std::fs::read(harness.files().store()).expect("the store"),
        store
    );
    harness.finish().await;
}
