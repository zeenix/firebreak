//! The lifecycle of an allowance as the owner sees it: funded, shown, and taken back.

use firebreak_core::journal::{Action, Actor, Outcome, Stage};
use firebreak_core::store::{Progress, VoucherState};
use firebreak_owner::{Error, Reclaim, create_allowance, reclaim, status};
use serde_json::json;

use crate::harness::{GENESIS, Harness};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_allowance_is_funded_shown_and_taken_back_in_full() {
    let harness = Harness::start(true).await;
    let ctx = &harness.ctx;

    // Before anything is funded the wallet holds the genesis allocation and nothing else.
    let before = status(ctx).await.expect("status");
    assert_eq!(before.status.wallet.balance, GENESIS);
    assert_eq!(before.status.wallet.outputs.len(), 1);
    assert!(before.status.allowances.is_empty());
    let genesis = before.status.wallet.outputs[0].id;

    // Funding: 100 sparks out of 1,000, as four vouchers, confirmed.
    let funded = create_allowance(ctx, &harness.request(&[50, 20, 20, 10], true))
        .await
        .expect("fund the allowance");
    assert!(funded.confirmed_height.is_some());
    assert_eq!(funded.allowance, hex::encode(&funded.funding_txid.0[..8]));
    let lines: Vec<(u64, VoucherState)> =
        funded.vouchers.iter().map(|v| (v.qty, v.state)).collect();
    assert_eq!(
        lines,
        [
            (50, VoucherState::Unspent),
            (20, VoucherState::Unspent),
            (20, VoucherState::Unspent),
            (10, VoucherState::Unspent)
        ]
    );
    let shown = funded.to_string();
    assert!(shown.contains(&funded.allowance), "{shown}");
    assert!(
        shown.contains(&hex::encode(funded.funding_txid.0)),
        "{shown}"
    );
    for voucher in &funded.vouchers {
        assert!(shown.contains(&hex::encode(voucher.id)), "{shown}");
    }
    assert!(
        shown.contains("50 sparks") && shown.contains("unspent"),
        "{shown}"
    );

    // The delegate's package is written, and holds the vouchers the owner saved.
    assert_eq!(funded.package, harness.files().package(&funded.allowance));
    let published = harness.vouchers(&funded.allowance);
    let ids: Vec<[u8; 32]> = funded.vouchers.iter().map(|v| v.id).collect();
    assert_eq!(published.iter().map(|v| v.id()).collect::<Vec<_>>(), ids);
    let package: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&funded.package).expect("the package"))
            .expect("json");
    let mut keys: Vec<&str> = package
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "allowance",
            "delegate",
            "funding_txid",
            "merchant",
            "owner",
            "version",
            "vouchers"
        ]
    );
    let owner_store = harness.files().load().expect("the store");
    let text = serde_json::to_string(&package).expect("json");
    assert!(!text.contains(&hex::encode(owner_store.wallet_seed)));
    assert!(!text.contains(&hex::encode(owner_store.authority_key.to_bytes())));
    assert!(!text.contains("opening") && !text.contains("qty_blinding"));

    // One journal line says what was sent, to whom, and what the node answered.
    let journal = harness.journal();
    assert_eq!(journal.len(), 1);
    let entry = &journal[0];
    assert_eq!(
        (entry.actor, entry.action.clone()),
        (Actor::Owner, Action::Fund)
    );
    assert_eq!(
        (entry.stage, entry.outcome),
        (Stage::Node, Outcome::Accepted)
    );
    assert_eq!(entry.txid, Some(funded.funding_txid));
    assert_eq!(entry.inputs, [genesis]);
    assert!(entry.tx.as_ref().is_some_and(|bytes| !bytes.is_empty()));
    assert_eq!(entry.error, None);
    let note = entry.note.as_deref().expect("a note");
    assert_eq!(
        note,
        format!("fund allowance {} with 4 vouchers", funded.allowance)
    );

    // The snapshot the dashboard reads: 900 left in the wallet, four vouchers unspent.
    let after = status(ctx).await.expect("status");
    let snapshot = harness.snapshot();
    assert_eq!(snapshot.wallet.balance, 900);
    assert_eq!(snapshot.wallet.outputs.len(), 1);
    assert_eq!(snapshot.wallet.outputs[0].qty, 900);
    assert_eq!(snapshot.wallet.outputs[0].state, VoucherState::Unspent);
    assert_eq!(snapshot.allowances.len(), 1);
    let allowance = &snapshot.allowances[0];
    assert_eq!(allowance.allowance, funded.allowance);
    assert_eq!(allowance.total, 100);
    assert_eq!(allowance.merchant, harness.merchant);
    assert_eq!(allowance.delegate, harness.delegate());
    assert_eq!(allowance.funding_txid, funded.funding_txid);
    assert!(
        allowance
            .vouchers
            .iter()
            .all(|v| v.state == VoucherState::Unspent && v.txid.is_none())
    );
    assert!(allowance.recovery.is_empty());
    let mut file = harness.snapshot();
    let mut same = after.status.clone();
    (file.updated, same.updated) = (0, 0);
    assert_eq!(file, same, "status prints what it wrote");

    // The shape the demonstration's scripts read.
    let value = serde_json::to_value(&after.status).expect("json");
    assert_eq!(value["wallet"]["balance"], "900");
    assert_eq!(value["allowances"][0]["vouchers"][0]["state"], "unspent");
    assert_eq!(value["allowances"][0]["vouchers"][0]["qty"], "50");
    assert_eq!(value["allowances"][0]["recovery"], json!([]));

    // Taking everything back: one recovery transaction, waited for.
    let reclaimed = reclaim(
        ctx,
        &Reclaim {
            wait: true,
            ..Reclaim::default()
        },
    )
    .await
    .expect("reclaim");
    assert_eq!(reclaimed.recoveries.len(), 1);
    assert!(reclaimed.skipped.is_empty());
    let recovery = &reclaimed.recoveries[0];
    assert!(recovery.is_revoked() && recovery.confirmed_height.is_some());
    assert_eq!(recovery.vouchers.len(), 4);
    assert!(
        recovery
            .vouchers
            .iter()
            .all(|v| v.state == VoucherState::Recovered)
    );
    let shown = reclaimed.to_string();
    assert!(
        shown.contains("revoked") && !shown.contains("recovery pending"),
        "{shown}"
    );

    let after = status(ctx).await.expect("status");
    assert_eq!(
        after.status.wallet.balance, GENESIS,
        "900 of change and 100 recovered"
    );
    let mut outputs: Vec<u64> = after.status.wallet.outputs.iter().map(|o| o.qty).collect();
    outputs.sort_unstable();
    assert_eq!(outputs, [100, 900]);
    let allowance = &after.status.allowances[0];
    for voucher in &allowance.vouchers {
        assert_eq!(voucher.state, VoucherState::Recovered);
        assert_eq!(voucher.txid, Some(recovery.txid));
    }
    assert_eq!(allowance.recovery.len(), 1);
    assert_eq!(allowance.recovery[0].txid, recovery.txid);
    assert_eq!(allowance.recovery[0].state, Progress::Confirmed);
    assert_eq!(allowance.recovery[0].vouchers.len(), 4);

    let journal = harness.journal();
    assert_eq!(journal.len(), 2);
    let entry = &journal[1];
    assert_eq!(
        (entry.actor, entry.action.clone()),
        (Actor::Owner, Action::Recover)
    );
    assert_eq!(
        (entry.stage, entry.outcome),
        (Stage::Node, Outcome::Accepted)
    );
    assert_eq!(entry.txid, Some(recovery.txid));
    let mut spent = entry.inputs.clone();
    spent.sort_unstable();
    let mut expected = ids.clone();
    expected.sort_unstable();
    assert_eq!(spent, expected);

    // There is nothing left to take back, and saying so changes nothing.
    let again = reclaim(ctx, &Reclaim::default())
        .await
        .expect("reclaim again");
    assert!(again.recoveries.is_empty());
    assert_eq!(again.skipped.len(), 4);
    assert!(
        again
            .skipped
            .iter()
            .all(|skip| skip.reason == "it was already recovered")
    );
    assert!(again.to_string().contains("nothing to recover"));
    assert_eq!(harness.journal().len(), 2);

    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recovery_is_pending_until_a_block_confirms_it() {
    let harness = Harness::start(false).await;
    let ctx = &harness.ctx;

    // Funding that is not in a block yet is pending, and the wallet has not been spent.
    let funded = create_allowance(ctx, &harness.request(&[50, 20, 20, 10], false))
        .await
        .expect("fund the allowance");
    assert_eq!(funded.confirmed_height, None);
    assert!(
        funded
            .vouchers
            .iter()
            .all(|v| v.state == VoucherState::FundingPending)
    );
    assert!(funded.to_string().contains("not confirmed yet"));
    let snapshot = harness.snapshot();
    assert_eq!(snapshot.wallet.balance, GENESIS);
    assert!(
        snapshot.allowances[0]
            .vouchers
            .iter()
            .all(|v| v.state == VoucherState::FundingPending)
    );

    harness.mint().await;
    let seen = status(ctx).await.expect("status");
    assert_eq!(seen.status.wallet.balance, 900);
    assert!(
        seen.status.allowances[0]
            .vouchers
            .iter()
            .all(|v| v.state == VoucherState::Unspent)
    );

    // A recovery that is not in a block yet is a request, not a revocation.
    let pending = reclaim(ctx, &Reclaim::default()).await.expect("reclaim");
    assert_eq!(pending.recoveries.len(), 1);
    let recovery = &pending.recoveries[0];
    assert!(!recovery.is_revoked());
    assert!(
        recovery
            .vouchers
            .iter()
            .all(|v| v.state == VoucherState::RecoveryPending)
    );
    let shown = pending.to_string();
    assert!(shown.contains("recovery pending"), "{shown}");
    assert!(!shown.contains("revoked:"), "{shown}");

    let snapshot = harness.snapshot();
    assert_eq!(
        snapshot.wallet.balance, 900,
        "the recovered output is not in a block yet"
    );
    let allowance = &snapshot.allowances[0];
    for voucher in &allowance.vouchers {
        assert_eq!(voucher.state, VoucherState::RecoveryPending);
        assert_eq!(voucher.txid, Some(recovery.txid));
    }
    assert_eq!(allowance.recovery.len(), 1);
    assert_eq!(allowance.recovery[0].state, Progress::Pending);

    // Asking again sends nothing: every voucher already has a recovery on its way.
    let again = reclaim(ctx, &Reclaim::default())
        .await
        .expect("reclaim again");
    assert!(again.recoveries.is_empty());
    assert_eq!(again.skipped.len(), 4);
    assert!(
        again
            .skipped
            .iter()
            .all(|skip| skip.reason.contains("a recovery is already pending"))
    );
    assert_eq!(
        harness.journal().len(),
        2,
        "only the funding and the first recovery were sent"
    );
    let still = status(ctx).await.expect("status");
    assert!(
        still.status.allowances[0]
            .vouchers
            .iter()
            .all(|v| v.state == VoucherState::RecoveryPending)
    );

    // A block confirms it, and only then are the vouchers recovered.
    harness.mint().await;
    let done = status(ctx).await.expect("status");
    assert_eq!(done.status.wallet.balance, GENESIS);
    let allowance = &done.status.allowances[0];
    assert!(
        allowance
            .vouchers
            .iter()
            .all(|v| v.state == VoucherState::Recovered)
    );
    assert_eq!(allowance.recovery[0].state, Progress::Confirmed);

    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_funding_that_is_not_confirmed_in_time_stays_pending() {
    let mut harness = Harness::start(false).await;
    harness.ctx.confirmation_timeout = std::time::Duration::from_millis(600);

    let error = create_allowance(&harness.ctx, &harness.request(&[50, 20], true))
        .await
        .expect_err("no block comes");
    assert!(matches!(error, Error::Unconfirmed { .. }), "{error}");
    let shown = error.to_string();
    assert!(
        shown.contains("not confirmed") && shown.contains("pending"),
        "{shown}"
    );

    // The funding is in the node's mempool, so the vouchers are pending, and the snapshot says so.
    let snapshot = harness.snapshot();
    assert!(
        snapshot.allowances[0]
            .vouchers
            .iter()
            .all(|v| v.state == VoucherState::FundingPending)
    );
    assert!(
        harness
            .files()
            .package(&snapshot.allowances[0].allowance)
            .exists()
    );

    harness.mint().await;
    let seen = status(&harness.ctx).await.expect("status");
    assert!(
        seen.status.allowances[0]
            .vouchers
            .iter()
            .all(|v| v.state == VoucherState::Unspent)
    );
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_restores_a_delegation_package_that_was_never_written() {
    let harness = Harness::start(true).await;
    let funded = create_allowance(&harness.ctx, &harness.request(&[50, 20], true))
        .await
        .expect("fund the allowance");
    let written = std::fs::read(&funded.package).expect("the package");

    // Suppose the program stopped before it wrote the package: the node has the allowance, and
    // the delegate has nothing to pay from. Status writes the same package again.
    std::fs::remove_file(&funded.package).expect("remove the package");
    status(&harness.ctx).await.expect("status");
    assert_eq!(
        std::fs::read(&funded.package).expect("the package"),
        written
    );
    harness.finish().await;
}
