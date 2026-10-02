//! Recovering vouchers while the delegate may be redeeming them.

use firebreak_core::chain::TxState;
use firebreak_core::journal::{Action, Outcome};
use firebreak_core::store::{Progress, VoucherState};
use firebreak_owner::{Error, Reclaim, create_allowance, reclaim, status};

use crate::harness::Harness;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vouchers_the_delegate_redeemed_are_left_alone_and_the_rest_come_back() {
    let harness = Harness::start(true).await;
    let ctx = &harness.ctx;
    let funded = create_allowance(ctx, &harness.request(&[50, 20, 20, 10], true))
        .await
        .expect("fund the allowance");

    // The delegate pays 60 with the 50 and the 10.
    let vouchers = harness.vouchers(&funded.allowance);
    let redemption = harness.redeem(&[&vouchers[0], &vouchers[3]]).await;
    harness.confirmed(&redemption).await;

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
    let recovered: Vec<u64> = reclaimed.recoveries[0]
        .vouchers
        .iter()
        .map(|v| v.qty)
        .collect();
    assert_eq!(recovered, [20, 20]);
    assert!(reclaimed.recoveries[0].is_revoked());
    let mut skipped: Vec<u64> = reclaimed.skipped.iter().map(|skip| skip.qty).collect();
    skipped.sort_unstable();
    assert_eq!(skipped, [10, 50]);
    assert!(
        reclaimed
            .skipped
            .iter()
            .all(|skip| skip.reason == "the delegate already redeemed it")
    );

    // The wallet holds what the demonstration promises: 900 of change and 40 recovered.
    let seen = status(ctx).await.expect("status");
    assert_eq!(seen.status.wallet.balance, 940);
    let states: Vec<VoucherState> = seen.status.allowances[0]
        .vouchers
        .iter()
        .map(|v| v.state)
        .collect();
    assert_eq!(
        states,
        [
            VoucherState::Redeemed,
            VoucherState::Recovered,
            VoucherState::Recovered,
            VoucherState::Redeemed
        ]
    );
    // A redeemed voucher names the transaction that spent it, which is not the owner's.
    let spenders: Vec<_> = seen.status.allowances[0]
        .vouchers
        .iter()
        .map(|v| v.txid)
        .collect();
    assert_eq!(spenders[0], Some(redemption));
    assert_eq!(spenders[3], Some(redemption));
    assert_eq!(spenders[1], Some(reclaimed.recoveries[0].txid));
    let unspent = seen.status.allowances[0]
        .vouchers
        .iter()
        .filter(|v| v.state == VoucherState::Unspent)
        .count();
    assert_eq!(unspent, 0);
    assert_eq!(seen.status.allowances[0].recovery[0].vouchers.len(), 2);
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pending_redemption_makes_the_node_refuse_and_nothing_changes() {
    let harness = Harness::start(false).await;
    let ctx = &harness.ctx;
    let funded = create_allowance(ctx, &harness.request(&[50, 20, 20, 10], false))
        .await
        .expect("fund the allowance");
    harness.mint().await;

    // The delegate's redemption of the 50 waits in the mempool. The owner cannot see it: the
    // voucher is still unspent in every block.
    let vouchers = harness.vouchers(&funded.allowance);
    let redemption = harness.redeem(&[&vouchers[0]]).await;
    let error = reclaim(ctx, &Reclaim::default())
        .await
        .expect_err("the node refuses");
    assert!(matches!(error, Error::Contested(_)), "{error}");
    let shown = error.to_string();
    assert!(shown.starts_with("node:"), "{shown}");
    assert!(shown.contains("Merkle proof is invalid"), "{shown}");
    assert!(shown.contains("still pending"), "{shown}");

    // Nothing was taken and nothing was recorded: every voucher is unspent, with no recovery.
    let snapshot = harness.snapshot();
    let allowance = &snapshot.allowances[0];
    assert!(
        allowance
            .vouchers
            .iter()
            .all(|v| v.state == VoucherState::Unspent && v.txid.is_none())
    );
    assert!(allowance.recovery.is_empty());
    let attempts: Vec<_> = harness
        .journal()
        .into_iter()
        .filter(|e| e.action == Action::Recover)
        .collect();
    assert_eq!(
        attempts.len(),
        2,
        "the refusal and the refused retry are both journaled"
    );
    assert!(
        attempts
            .iter()
            .all(|entry| entry.outcome == Outcome::Rejected)
    );

    // Once the redemption is in a block, the owner takes back the other three.
    harness.mint().await;
    let state = harness
        .ctx
        .chain
        .tx_state(&redemption)
        .await
        .expect("state");
    assert!(matches!(state, TxState::Confirmed { .. }), "{state:?}");
    let reclaimed = reclaim(ctx, &Reclaim::default()).await.expect("reclaim");
    assert_eq!(reclaimed.recoveries.len(), 1);
    assert_eq!(reclaimed.recoveries[0].vouchers.len(), 3);
    assert_eq!(reclaimed.skipped.len(), 1);
    harness.mint().await;
    let done = status(ctx).await.expect("status");
    assert_eq!(done.status.wallet.balance, 900 + 50);
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_allowance_and_vouchers_can_be_named() {
    let harness = Harness::start(true).await;
    let ctx = &harness.ctx;
    let first = create_allowance(ctx, &harness.request(&[50, 20], true))
        .await
        .expect("fund");
    let second = create_allowance(ctx, &harness.request(&[10, 10], true))
        .await
        .expect("fund");

    // One voucher by the start of its id.
    let chosen = hex::encode(first.vouchers[1].id);
    let request = Reclaim {
        vouchers: vec![chosen[..12].to_owned()],
        wait: true,
        ..Reclaim::default()
    };
    let reclaimed = reclaim(ctx, &request).await.expect("reclaim one voucher");
    assert_eq!(reclaimed.recoveries.len(), 1);
    assert_eq!(reclaimed.recoveries[0].vouchers.len(), 1);
    assert_eq!(reclaimed.recoveries[0].vouchers[0].id, first.vouchers[1].id);
    assert!(
        reclaimed.skipped.is_empty(),
        "only the named voucher is considered"
    );

    // One allowance by its id.
    let request = Reclaim {
        allowance: Some(second.allowance.clone()),
        wait: true,
        ..Reclaim::default()
    };
    let reclaimed = reclaim(ctx, &request).await.expect("reclaim an allowance");
    assert_eq!(reclaimed.recoveries.len(), 1);
    assert_eq!(reclaimed.recoveries[0].allowance, second.allowance);
    assert_eq!(reclaimed.recoveries[0].vouchers.len(), 2);

    let seen = status(ctx).await.expect("status");
    let by_allowance = |id: &str| {
        let record = seen
            .status
            .allowances
            .iter()
            .find(|a| a.allowance == id)
            .expect("listed");
        record.vouchers.iter().map(|v| v.state).collect::<Vec<_>>()
    };
    assert_eq!(
        by_allowance(&first.allowance),
        [VoucherState::Unspent, VoucherState::Recovered]
    );
    assert_eq!(
        by_allowance(&second.allowance),
        [VoucherState::Recovered; 2]
    );
    assert!(
        seen.status.allowances[0]
            .recovery
            .iter()
            .all(|r| r.state == Progress::Confirmed)
    );

    // Names that match nothing, or too much, are refused before anything is sent.
    for (name, why) in [("zz", "not a voucher id"), ("", "not a voucher id")] {
        let request = Reclaim {
            vouchers: vec![name.to_owned()],
            ..Reclaim::default()
        };
        let error = reclaim(ctx, &request).await.expect_err("a bad name");
        assert!(matches!(error, Error::Refused(_)), "{error}");
        assert!(error.to_string().contains(why), "{error}");
    }
    let request = Reclaim {
        vouchers: vec!["0".repeat(64)],
        ..Reclaim::default()
    };
    let error = reclaim(ctx, &request).await.expect_err("no such voucher");
    assert!(error.to_string().contains("no voucher"), "{error}");
    let request = Reclaim {
        allowance: Some("0".repeat(16)),
        ..Reclaim::default()
    };
    let error = reclaim(ctx, &request).await.expect_err("no such allowance");
    assert!(error.to_string().contains("no allowance"), "{error}");
    harness.finish().await;
}
