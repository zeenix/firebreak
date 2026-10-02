//! Paying: which vouchers are redeemed, what is refused, and what is journaled.

use std::sync::Arc;
use std::time::Duration;

use firebreak_agent::Agent;
use firebreak_agent::pay::Stage;
use firebreak_core::NETWORK;
use firebreak_core::chain::{Chain, ContractState};
use firebreak_core::devnet::{Parties, rng};
use firebreak_core::journal::{self, Action, Actor, Outcome};
use firebreak_core::store::VoucherState;

use crate::fixture::World;

const WAIT: Duration = Duration::from_secs(30);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sixty_sparks_redeem_the_fifty_and_the_ten_and_what_is_left_cannot_pay_more() {
    let world = World::start().await;
    let [fifty, twenty, other_twenty, ten] = world.funded.ids().try_into().expect("four vouchers");

    let payment = world
        .agent
        .pay(&world.request(60), Some(WAIT))
        .await
        .expect("the payment");
    let paid: Vec<[u8; 32]> = payment.vouchers.iter().map(|voucher| voucher.id).collect();
    assert_eq!(paid, [fifty, ten]);
    assert_eq!(payment.amount(), 60);
    assert_eq!(payment.state, VoucherState::Redeemed);
    assert!(payment.height.is_some());
    assert!(payment.warnings.is_empty(), "{:?}", payment.warnings);

    // The chain agrees: the 50 and the 10 are spent by this transaction, the 20s are not.
    let ids = [fifty, twenty, other_twenty, ten];
    let states = world.net.chain.states(&ids).await.expect("states");
    for (index, state) in states.iter().enumerate() {
        match (index, state) {
            (0 | 3, ContractState::Spent { txid, .. }) => assert_eq!(*txid, payment.txid),
            (1 | 2, ContractState::Unspent(_)) => {}
            (index, other) => panic!("voucher {index} is {other}"),
        }
    }
    let redeemed = [
        (50, VoucherState::Redeemed, Some(payment.txid)),
        (20, VoucherState::Unspent, None),
        (20, VoucherState::Unspent, None),
        (10, VoucherState::Redeemed, Some(payment.txid)),
    ];
    assert_eq!(world.recorded().await, redeemed);

    // What is left, 20 and 20, cannot pay 110, cannot make 30, and does not pay a merchant that
    // is not the allowance's. Each refusal says where it stopped.
    let refusal = |error: firebreak_agent::PayError| (error.stage, error.message);
    let too_much = world.agent.pay(&world.request(110), None).await;
    let (stage, message) = refusal(too_much.expect_err("more than the vouchers left"));
    assert_eq!(
        (stage, message.as_str()),
        (Stage::Selection, "insufficient authority")
    );
    let odd = world.agent.pay(&world.request(30), None).await;
    let (stage, message) = refusal(odd.expect_err("no exact combination"));
    let denominations = "no exact combination of vouchers for 30 (denominations: 20, 20)";
    assert_eq!((stage, message.as_str()), (Stage::Selection, denominations));
    let nothing = world.agent.pay(&world.request(0), None).await;
    let (stage, message) = refusal(nothing.expect_err("zero"));
    assert_eq!(
        (stage, message.as_str()),
        (Stage::Selection, "the amount must be greater than zero")
    );
    let mut elsewhere = world.request(20);
    elsewhere.merchant = Parties::new(&mut rng(99))
        .merchant_address()
        .to_bech32(NETWORK);
    let error = world
        .agent
        .pay(&elsewhere, None)
        .await
        .expect_err("another merchant");
    assert_eq!(error.stage, Stage::Policy);
    assert!(error.message.contains("pays only"), "{}", error.message);
    assert!(
        error.message.contains("the chain would refuse"),
        "{}",
        error.message
    );
    let mut garbled = world.request(20);
    garbled.merchant = "not an address".to_owned();
    let error = world
        .agent
        .pay(&garbled, None)
        .await
        .expect_err("no address");
    assert_eq!(error.stage, Stage::Policy);

    // None of that reserved or submitted anything: the records are as they were, and the journal
    // holds the one redemption and nothing about its amounts.
    assert_eq!(world.recorded().await, redeemed);
    let journal = journal::read_all(&world.files.journal()).expect("the journal");
    assert_eq!(journal.skipped, 0);
    let [entry] = journal.entries.as_slice() else {
        panic!("expected one entry, got {}", journal.entries.len());
    };
    assert_eq!(
        (entry.actor, entry.stage),
        (Actor::Agent, journal::Stage::Node)
    );
    assert_eq!(
        (&entry.action, entry.outcome),
        (&Action::Redeem, Outcome::Accepted)
    );
    assert_eq!(entry.txid, Some(payment.txid));
    assert_eq!(entry.inputs, [fifty, ten]);
    assert!(entry.tx.as_ref().is_some_and(|tx| !tx.is_empty()));
    assert_eq!(entry.error, None);
    let note = format!(
        "redeem 2 voucher(s) of allowance {}",
        world.funded.allowance()
    );
    assert_eq!(entry.note.as_deref(), Some(note.as_str()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_redemption_no_block_holds_yet_reserves_its_vouchers_until_one_does() {
    let world = World::start_manual().await;
    let payment = world
        .agent
        .pay(&world.request(60), None)
        .await
        .expect("the payment");
    assert_eq!(
        (payment.state, payment.height),
        (VoucherState::RedemptionPending, None)
    );

    // The vouchers are saved as reserved before anything else can pick them.
    let txid = Some(payment.txid);
    let reserved = [
        (50, VoucherState::RedemptionPending, txid),
        (20, VoucherState::Unspent, None),
        (20, VoucherState::Unspent, None),
        (10, VoucherState::RedemptionPending, txid),
    ];
    assert_eq!(world.recorded().await, reserved);

    // The node holds the transaction in its mempool, so asking it changes nothing, and the
    // reserved vouchers are not there to pay with: 40 are left.
    world.agent.status().await.expect("a status");
    assert_eq!(world.recorded().await, reserved);
    let error = world
        .agent
        .pay(&world.request(60), None)
        .await
        .expect_err("only the 20s are left");
    assert_eq!(error.message, "insufficient authority");
    assert_eq!(world.recorded().await, reserved);

    // A block confirms the redemption, and the vouchers are redeemed for good.
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
async fn a_node_that_cannot_be_asked_stops_a_payment_before_it_reserves_anything() {
    let world = World::start().await;
    let before = world.recorded().await;
    let chain = Chain::connect("http://127.0.0.1:1").expect("a client");
    let blind = Agent::new(world.files.clone(), chain);

    let error = blind
        .pay(&world.request(60), Some(WAIT))
        .await
        .expect_err("no node to ask");
    assert_eq!(error.stage, Stage::Node);
    assert!(
        error.message.starts_with("nothing was submitted"),
        "{}",
        error.message
    );
    assert_eq!(error.txid, None);
    assert_eq!(world.recorded().await, before);
    let journal = journal::read_all(&world.files.journal()).expect("the journal");
    assert!(journal.entries.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_allowance_is_named_when_there_are_several_and_may_be_left_out_when_there_is_one() {
    let mut world = World::start().await;

    let mut request = world.request(60);
    request.allowance = Some("0000000000000000".to_owned());
    let error = world
        .agent
        .pay(&request, None)
        .await
        .expect_err("no such allowance");
    assert_eq!(error.stage, Stage::Input);
    assert_eq!(error.message, "no allowance 0000000000000000 is imported");

    // With a second allowance, leaving the allowance out is ambiguous.
    let second = world.fund_another(&[30, 30]).await;
    let error = world
        .agent
        .pay(&world.request(60), None)
        .await
        .expect_err("which allowance?");
    assert_eq!(error.stage, Stage::Input);
    assert!(
        error.message.starts_with("2 allowances are imported"),
        "{}",
        error.message
    );
    assert!(
        error.message.contains(&second.allowance()),
        "{}",
        error.message
    );

    // Naming it pays from it, and only from it.
    request.allowance = Some(second.allowance());
    let payment = world
        .agent
        .pay(&request, Some(WAIT))
        .await
        .expect("the payment");
    let paid: Vec<u64> = payment.vouchers.iter().map(|voucher| voucher.qty).collect();
    assert_eq!(paid, [30, 30]);
    assert_eq!(payment.state, VoucherState::Redeemed);
    assert!(
        world
            .recorded()
            .await
            .iter()
            .all(|(_, state, _)| *state == VoucherState::Unspent)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn payments_made_through_two_handles_never_reserve_the_same_voucher() {
    let world = World::start().await;
    // Two handles on the same files and node stand for two processes, such as the server and the
    // command line, which share no memory and only the turn on disk.
    let handles = [
        Arc::clone(&world.agent),
        Arc::new(Agent::new(world.files.clone(), world.net.chain.clone())),
    ];
    let payments: Vec<_> = (0..4)
        .map(|index| {
            let agent = Arc::clone(&handles[index % 2]);
            let request = world.request(20);
            tokio::spawn(async move { agent.pay(&request, Some(WAIT)).await })
        })
        .collect();
    let mut results = Vec::new();
    for payment in payments {
        results.push(payment.await.expect("the payment task"));
    }

    // Two vouchers of 20 pay four requests for 20: two succeed, with a voucher each, and the
    // others find nothing left that makes 20.
    let paid: Vec<_> = results
        .iter()
        .filter_map(|result| result.as_ref().ok())
        .collect();
    let refused: Vec<_> = results
        .iter()
        .filter_map(|result| result.as_ref().err())
        .collect();
    assert_eq!((paid.len(), refused.len()), (2, 2), "{results:?}");
    assert_ne!(paid[0].vouchers, paid[1].vouchers);
    assert_ne!(paid[0].txid, paid[1].txid);
    for payment in &paid {
        assert_eq!(payment.state, VoucherState::Redeemed);
    }
    for error in &refused {
        assert_eq!(error.stage, Stage::Selection);
        assert_eq!(
            error.message,
            "no exact combination of vouchers for 20 (denominations: 50, 10)"
        );
    }
    let states: Vec<_> = world
        .recorded()
        .await
        .into_iter()
        .map(|(_, state, _)| state)
        .collect();
    assert_eq!(
        states,
        [
            VoucherState::Unspent,
            VoucherState::Redeemed,
            VoucherState::Redeemed,
            VoucherState::Unspent
        ]
    );
}
