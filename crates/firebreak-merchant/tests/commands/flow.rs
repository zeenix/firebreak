//! The merchant's view of the payments vouchers make: found, opened, and spent.

use firebreak_core::journal::{Action, Actor, Outcome, Stage};
use firebreak_core::store::{MerchantStore, Progress};
use firebreak_core::{NETWORK, Payout, build, wallet};
use firebreak_merchant::{inspect, spend};
use flamekd::util;
use flamepayments::{Account, OutputSpec, prepare_output};
use flamevm::FLAME_FLAVOR;
use rand::rngs::OsRng;

use crate::harness::Harness;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn payments_are_found_opened_and_spent_to_fresh_addresses() {
    let harness = Harness::start(true).await;
    let ctx = &harness.ctx;

    // Nothing has been paid yet, and there is nothing to spend.
    let empty = inspect(ctx).await.expect("inspect");
    assert!(empty.status.receipts.is_empty() && empty.status.spends.is_empty());
    assert_eq!(empty.status.balance, 0);
    assert_eq!(empty.status.address, harness.address());
    assert!(empty.to_string().contains("no payments received yet"));
    let nothing = spend(ctx, true).await.expect("spend");
    assert!(nothing.spent.is_none());
    assert!(nothing.to_string().contains("nothing to spend"));
    assert!(harness.journal().is_empty());

    // The owner funds an allowance of 100, and the delegate redeems the 50 and the 10.
    let funded = harness.fund(&[50, 20, 20, 10]).await;
    let vouchers = harness.vouchers(&funded.allowance);
    let redemption = harness.pay(&[&vouchers[0], &vouchers[3]]).await;

    // Both payments are found, with their receipts opened: amount, memo, and where they are.
    let seen = inspect(ctx).await.expect("inspect");
    assert!(seen.skipped.is_empty());
    assert_eq!(seen.status.balance, 60);
    let mut receipts = seen.status.receipts.clone();
    receipts.sort_by_key(|receipt| receipt.qty);
    let quantities: Vec<u64> = receipts.iter().map(|receipt| receipt.qty).collect();
    assert_eq!(quantities, [10, 50]);
    for receipt in &receipts {
        assert_eq!(receipt.txid, redemption);
        assert_eq!(receipt.memo, "firebreak voucher");
        assert!(!receipt.spent && receipt.spent_txid.is_none());
        assert!(receipt.height > 0);
    }
    let shown = seen.to_string();
    assert!(
        shown.contains("50 sparks") && shown.contains("10 sparks"),
        "{shown}"
    );
    assert!(shown.contains("\"firebreak voucher\""), "{shown}");
    assert!(shown.contains(&hex::encode(redemption.0)), "{shown}");

    // The snapshot the dashboard reads is what inspect printed, in the shape the demo's scripts
    // read.
    let (mut file, mut printed) = (harness.snapshot(), seen.status.clone());
    (file.updated, printed.updated) = (0, 0);
    assert_eq!(file, printed);
    let value = serde_json::to_value(&seen.status).expect("json");
    assert_eq!(value["balance"], "60");
    assert_eq!(value["address"], harness.address().to_bech32(NETWORK));
    assert!(
        value["receipts"]
            .as_array()
            .is_some_and(|all| all.len() == 2)
    );
    assert_eq!(value["receipts"][0]["spent"], false);
    assert_eq!(value["spends"], serde_json::json!([]));

    // Spending moves both payments to a fresh address in one transaction, and the balance stays.
    let report = spend(ctx, true).await.expect("spend");
    let spent = report.spent.as_ref().expect("a spend");
    assert_eq!((spent.qty, spent.receipts), (60, 2));
    assert!(spent.confirmed_height.is_some());
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    let store = MerchantStore::load(&harness.files().store()).expect("the store");
    let account = store.account().expect("an account");
    let second = account.address_at(util::RECEIVING, 1).expect("an address");
    assert_eq!(spent.to, second, "the fresh address is the wallet's next");
    assert_ne!(spent.to, harness.address(), "never the published one");
    assert_eq!(store.next_index, 2);
    let shown = report.to_string();
    assert!(
        shown.contains("60 sparks") && shown.contains("confirmed in block"),
        "{shown}"
    );

    let after = inspect(ctx).await.expect("inspect");
    assert_eq!(
        after.status.balance, 60,
        "the spend leaves the balance as it was"
    );
    assert!(
        after
            .status
            .receipts
            .iter()
            .all(|r| r.spent && r.spent_txid == Some(spent.txid))
    );
    assert_eq!(after.status.spends.len(), 1);
    let record = &after.status.spends[0];
    assert_eq!(
        (record.txid, record.qty, record.to),
        (spent.txid, 60, second)
    );
    assert_eq!(record.state, Progress::Confirmed);
    // The 60 is one output at the fresh address.
    let synced = wallet::sync(&ctx.chain, &account, 1..2, 0..0)
        .await
        .expect("sync");
    let unspent: Vec<u64> = synced
        .outputs
        .iter()
        .filter(|output| output.spent.is_none())
        .map(|output| output.qty)
        .collect();
    assert_eq!(unspent, [60]);

    // One journal line says what was sent and what the node answered, with no amount in it.
    let journal = harness.journal();
    assert_eq!(journal.len(), 1);
    let entry = &journal[0];
    assert_eq!(
        (entry.actor, entry.action.clone()),
        (Actor::Merchant, Action::Spend)
    );
    assert_eq!(
        (entry.stage, entry.outcome),
        (Stage::Node, Outcome::Accepted)
    );
    assert_eq!(entry.txid, Some(spent.txid));
    let mut inputs = entry.inputs.clone();
    inputs.sort_unstable();
    let mut ids: Vec<[u8; 32]> = after.status.receipts.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    assert_eq!(inputs, ids);
    assert!(entry.tx.as_ref().is_some_and(|bytes| !bytes.is_empty()));
    assert_eq!(entry.error, None);
    assert_eq!(
        entry.note.as_deref(),
        Some("spend 2 received payment(s) to a fresh address")
    );

    // There is nothing left to spend, and saying so sends nothing.
    let again = spend(ctx, true).await.expect("spend again");
    assert!(again.spent.is_none());
    assert_eq!(harness.journal().len(), 1);

    // A payment that comes later is found and spent alone, to the next fresh address.
    harness.pay(&[&vouchers[1]]).await;
    let seen = inspect(ctx).await.expect("inspect");
    assert_eq!((seen.status.receipts.len(), seen.status.balance), (3, 80));
    let report = spend(ctx, true).await.expect("spend");
    let spent = report.spent.expect("a spend");
    assert_eq!((spent.qty, spent.receipts), (20, 1));
    let third = account.address_at(util::RECEIVING, 2).expect("an address");
    assert_eq!(spent.to, third);
    let seen = inspect(ctx).await.expect("inspect");
    assert_eq!(seen.status.balance, 80);
    assert_eq!(seen.status.spends.len(), 2);
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_payment_whose_receipt_does_not_open_is_reported_and_never_spent() {
    let harness = Harness::start(true).await;
    let ctx = &harness.ctx;
    let funded = harness.fund(&[50, 20]).await;
    let vouchers = harness.vouchers(&funded.allowance);
    harness.pay(&[&vouchers[0]]).await;

    // Someone also pays the merchant's address, with a token whose note is sealed to a stranger,
    // so that the merchant cannot open it. The owner's wallet pays here, which is as good as any.
    let owner = firebreak_owner::Files::new(harness.dir.path().to_owned())
        .load()
        .expect("the owner's store");
    let account = owner.account().expect("an account");
    let synced = wallet::sync(&ctx.chain, &account, 0..1, owner.change_range())
        .await
        .expect("sync the owner");
    let funds = synced
        .outputs
        .iter()
        .find(|output| output.spent.is_none())
        .expect("the owner's change");
    let proofs = ctx.chain.fresh_proofs(&[funds.id]).await.expect("proofs");
    let input = funds
        .to_input(&account, proofs[0].clone())
        .expect("an input");
    let stranger = Account::from_seed(&[0x42; 64], NETWORK, 0)
        .expect("an account")
        .address_at(util::RECEIVING, 0)
        .expect("an address");
    let spec = |address, qty| OutputSpec {
        address,
        qty,
        flv: FLAME_FLAVOR,
        memo: Vec::new(),
    };
    let crafted = prepare_output(&spec(stranger, 5), &mut OsRng).expect("prepare");
    let back = account.address_at(util::CHANGE, 7).expect("an address");
    let change = prepare_output(&spec(back, funds.qty - 5), &mut OsRng).expect("prepare");
    let payouts = [
        Payout::Wallet {
            to: harness.address().spending_key().compress(),
            prepared: &crafted,
        },
        Payout::Wallet {
            to: back.spending_key().compress(),
            prepared: &change,
        },
    ];
    let unsigned = build::funding(std::slice::from_ref(&input), &payouts, 0).expect("build");
    let tx = build::sign(unsigned, &[input.signing_key()]).expect("sign");
    let bytes = build::package(tx, vec![input.proof().clone()]).expect("package");
    let txid = ctx.chain.submit(bytes).await.expect("the node takes it");
    harness.confirmed(&txid).await;

    // The merchant reports the output it cannot use, and never counts or spends it.
    let seen = inspect(ctx)
        .await
        .expect("inspect does not fail on a crafted payment");
    assert_eq!(seen.skipped.len(), 1);
    assert_eq!(seen.status.balance, 50);
    assert_eq!(seen.status.receipts.len(), 1);
    let shown = seen.to_string();
    assert!(
        shown.contains("warning:") && shown.contains("was left out"),
        "{shown}"
    );

    let spent = spend(ctx, true)
        .await
        .expect("spend")
        .spent
        .expect("a spend");
    assert_eq!((spent.qty, spent.receipts), (50, 1));
    let seen = inspect(ctx).await.expect("inspect");
    assert_eq!((seen.skipped.len(), seen.status.balance), (1, 50));
    harness.finish().await;
}
