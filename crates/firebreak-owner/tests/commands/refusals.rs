//! Requests the owner refuses, and the limits of what one funding transaction proves.

use std::fs;

use firebreak_core::store::VoucherState;
use firebreak_core::{NETWORK, keys};
use firebreak_owner::{
    Error, MAX_PAYOUTS, Reclaim, create_allowance, parse_delegate, parse_merchant, parse_vouchers,
    reclaim, status,
};
use flamekd::{Network, util};
use flamepayments::Account;
use rand::rngs::OsRng;

use crate::harness::{GENESIS, Harness};

#[test]
fn amounts_are_positive_whole_numbers_of_sparks() {
    assert_eq!(
        parse_vouchers("50,20,20,10").expect("amounts"),
        [50, 20, 20, 10]
    );
    assert_eq!(parse_vouchers(" 5 , 6 ").expect("amounts"), [5, 6]);
    assert_eq!(
        parse_vouchers("18446744073709551615").expect("amounts"),
        [u64::MAX]
    );
    for text in [
        "",
        "0",
        "50,0",
        "-5",
        "+5",
        "5.5",
        "1e3",
        "5,,6",
        "5,",
        ",5",
        "abc",
        "50,x",
        "0x10",
        "18446744073709551616",
    ] {
        let error = parse_vouchers(text).expect_err(text);
        assert!(matches!(error, Error::Refused(_)), "{text:?}: {error}");
        assert!(
            error.to_string().contains("positive whole number"),
            "{text:?}: {error}"
        );
    }
}

#[test]
fn a_delegate_is_named_by_a_verification_key_and_never_by_a_secret() {
    let key = keys::verification_key(&keys::generate(&mut OsRng));
    let text = hex::encode(key.to_bytes());
    assert_eq!(parse_delegate(&text).expect("a key"), key);
    assert_eq!(parse_delegate(&format!(" {text}\n")).expect("a key"), key);

    for (bad, why) in [
        ("", "32 bytes"),
        ("not hex", "hexadecimal"),
        ("abcd", "32 bytes"),
        (&"ff".repeat(32), "not a valid Ristretto point"),
        (&format!("{text}00"), "32 bytes"),
        (&"00".repeat(32), "identity"),
    ] {
        let error = parse_delegate(bad).expect_err(bad);
        assert!(matches!(error, Error::Refused(_)), "{bad:?}: {error}");
        assert!(error.to_string().contains(why), "{bad:?}: {error}");
        assert!(
            bad.is_empty() || !error.to_string().contains(bad),
            "the text is not repeated"
        );
    }
}

#[test]
fn a_merchant_is_a_testnet_address() {
    let account = Account::from_seed(&[3; 64], NETWORK, 0).expect("an account");
    let address = account.address_at(util::RECEIVING, 0).expect("an address");
    let text = address.to_bech32(NETWORK);
    assert!(text.starts_with("tf1"));
    assert_eq!(parse_merchant(&text).expect("an address"), address);
    assert_eq!(
        parse_merchant(&format!("{text}\n")).expect("an address"),
        address
    );

    let mainnet = address.to_bech32(Network::Mainnet);
    for bad in ["", "tf1", "hello", &mainnet, &text[..text.len() - 1]] {
        let error = parse_merchant(bad).expect_err(bad);
        assert!(matches!(error, Error::Refused(_)), "{bad:?}: {error}");
        assert!(
            error.to_string().contains("merchant address"),
            "{bad:?}: {error}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_that_cannot_be_funded_are_refused_and_leave_nothing_behind() {
    let harness = Harness::start(false).await;
    let ctx = &harness.ctx;
    let store = harness.files().load().expect("the store");
    let before = fs::read(harness.files().store()).expect("the store file");

    let mut own_key = harness.request(&[50], false);
    own_key.delegate = store.authority();
    let mut free = harness.request(&[50, 0], false);
    free.vouchers = vec![50, 0];
    let cases = [
        (own_key, "the owner's own voucher authority key"),
        (harness.request(&[], false), "1 to 13 vouchers, not 0"),
        (harness.request(&[1; 14], false), "1 to 13 vouchers, not 14"),
        (free, "at least one spark"),
        (
            harness.request(&[u64::MAX, 1], false),
            "more than a transaction can hold",
        ),
        (
            harness.request(&[2_000], false),
            "insufficient funds: the wallet holds 1000 sparks and the allowance needs 2000",
        ),
        (
            harness.request(&[10; 13], false),
            "13 vouchers and the change make 14 outputs, and one funding transaction proves at \
             most 13; ask for at most 12 vouchers",
        ),
    ];
    for (request, why) in cases {
        let error = create_allowance(ctx, &request).await.expect_err(why);
        assert!(matches!(error, Error::Refused(_)), "{why}: {error}");
        assert!(error.to_string().starts_with("refused:"), "{error}");
        assert!(error.to_string().contains(why), "{why}: {error}");
    }

    // Not a byte of the store changed, no change address was issued, and nothing was published.
    assert_eq!(
        fs::read(harness.files().store()).expect("the store file"),
        before
    );
    assert_eq!(
        harness.files().load().expect("the store").next_change_index,
        0
    );
    assert!(!harness.files().journal().exists());
    assert!(!harness.files().status().exists());
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_funding_proves_thirteen_outputs_with_change_and_thirteen_vouchers_without() {
    assert_eq!(MAX_PAYOUTS, 13);

    // Twelve vouchers and the change make thirteen outputs.
    let harness = Harness::start(true).await;
    let funded = create_allowance(&harness.ctx, &harness.request(&[10; 12], true))
        .await
        .expect("fund twelve vouchers");
    assert_eq!(funded.vouchers.len(), 12);
    assert_eq!(harness.snapshot().wallet.balance, GENESIS - 120);
    assert_eq!(
        harness.files().load().expect("the store").next_change_index,
        1
    );
    harness.finish().await;

    // Spending the whole wallet output leaves no change, so thirteen vouchers fit.
    let harness = Harness::start(true).await;
    let mut amounts = vec![75; 12];
    amounts.push(100);
    assert_eq!(amounts.iter().sum::<u64>(), GENESIS);
    let funded = create_allowance(&harness.ctx, &harness.request(&amounts, true))
        .await
        .expect("fund thirteen vouchers");
    assert_eq!(funded.vouchers.len(), 13);
    assert!(
        funded
            .vouchers
            .iter()
            .all(|v| v.state == VoucherState::Unspent)
    );
    let seen = status(&harness.ctx).await.expect("status");
    assert_eq!(seen.status.wallet.balance, 0);
    assert!(seen.status.wallet.outputs.is_empty());
    assert_eq!(
        harness.files().load().expect("the store").next_change_index,
        0,
        "no change address is issued when nothing is left over"
    );

    // The wallet is empty, so nothing more can be funded.
    let error = create_allowance(&harness.ctx, &harness.request(&[1], false))
        .await
        .expect_err("nothing left");
    assert!(
        error.to_string().contains("the wallet holds 0 sparks"),
        "{error}"
    );

    // All thirteen come back in one recovery transaction.
    let reclaimed = reclaim(
        &harness.ctx,
        &Reclaim {
            wait: true,
            ..Reclaim::default()
        },
    )
    .await
    .expect("reclaim thirteen vouchers");
    assert_eq!(reclaimed.recoveries.len(), 1);
    assert_eq!(reclaimed.recoveries[0].vouchers.len(), 13);
    assert!(reclaimed.recoveries[0].is_revoked());
    let seen = status(&harness.ctx).await.expect("status");
    assert_eq!(seen.status.wallet.balance, GENESIS);
    harness.finish().await;
}
