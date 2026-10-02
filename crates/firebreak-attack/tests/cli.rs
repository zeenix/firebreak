//! The attack runner against a node served over JSON-RPC, as the command line drives it: the
//! adversary knows the vouchers only from their public descriptors, submits directly to the node,
//! and journals every attempt.

use std::time::Duration;

use firebreak_attack::run::{self, Context, Verdict};
use firebreak_attack::{ALL, Adversary, Attack};
use firebreak_core::chain::{Chain, ContractState};
use firebreak_core::devnet::{DENOMINATIONS, GENESIS_SPARKS, LocalNode, Parties, output, rng};
use firebreak_core::journal::{self, Action, Actor, Stage};
use firebreak_core::store::DelegationPackage;
use firebreak_core::{NETWORK, Payout, Voucher, VoucherPolicy, build, keys, wallet};
use flamekd::util;
use flamepayments::{Account, Opening, PreparedOutput, prepare_output};
use rand::rngs::StdRng;

const PATIENCE: Duration = Duration::from_secs(10);

/// Funds one voucher per denomination through the node, and returns the vouchers as a public
/// delegation package describes them, with the owner's openings.
async fn fund(
    node: &LocalNode,
    chain: &Chain,
    parties: &Parties,
    rng: &mut StdRng,
) -> (Vec<Voucher>, Vec<Opening>) {
    let merchant = parties.merchant_address();
    let synced = wallet::sync(chain, &parties.owner, 0..1, 0..0)
        .await
        .expect("sync the owner");
    let genesis = &synced.outputs[0];
    let policies: Vec<VoucherPolicy> = DENOMINATIONS.iter().map(|_| parties.policy(rng)).collect();
    let prepared: Vec<PreparedOutput> = DENOMINATIONS
        .iter()
        .map(|qty| prepare_output(&output(merchant, *qty), rng).expect("prepare"))
        .collect();
    let change_address = parties.owner.address_at(util::CHANGE, 0).expect("address");
    let change = prepare_output(
        &output(
            change_address,
            GENESIS_SPARKS - DENOMINATIONS.iter().sum::<u64>(),
        ),
        rng,
    )
    .expect("prepare");
    let mut payouts: Vec<Payout<'_>> = policies
        .iter()
        .zip(&prepared)
        .map(|(policy, prepared)| Payout::Voucher { policy, prepared })
        .collect();
    payouts.push(Payout::Wallet {
        to: change_address.spending_key().compress(),
        prepared: &change,
    });
    let proofs = chain.fresh_proofs(&[genesis.id]).await.expect("a proof");
    let input = genesis
        .to_input(&parties.owner, proofs[0].clone())
        .expect("an input");
    let unsigned = build::funding(std::slice::from_ref(&input), &payouts, 0).expect("funding");
    let txid = unsigned.log().txid();
    let created = build::outputs(unsigned.log());
    let vouchers: Vec<Voucher> = policies
        .iter()
        .zip(DENOMINATIONS)
        .map(|(policy, qty)| {
            let predicate = policy.predicate().expect("a predicate");
            let (contract, _) = created
                .iter()
                .find(|(contract, _)| contract.predicate.to_point() == predicate)
                .expect("a voucher");
            Voucher::new(*policy, contract.clone(), qty).expect("a voucher")
        })
        .collect();
    let tx = build::sign(unsigned, &[input.signing_key()]).expect("sign");
    let bytes = build::package(tx, proofs).expect("package");
    chain.submit(bytes).await.expect("the funding is admitted");
    node.mint();
    chain
        .wait_confirmed(&txid, PATIENCE)
        .await
        .expect("confirmed");

    // What the adversary sees is what the owner publishes: the package, decoded and checked.
    let package = DelegationPackage::from_vouchers(txid, &vouchers).expect("a package");
    let json = serde_json::to_vec(&package).expect("serialize");
    let public: DelegationPackage = serde_json::from_slice(&json).expect("deserialize");
    let vouchers = public.vouchers().expect("the public vouchers");
    (
        vouchers,
        prepared.iter().map(|prepared| prepared.opening).collect(),
    )
}

#[tokio::test]
async fn every_attack_through_the_runner_is_refused_and_journaled() {
    let mut rng = rng(41);
    let parties = Parties::new(&mut rng);
    let owner_address = parties
        .owner
        .address_at(util::RECEIVING, 0)
        .expect("owner address");
    let node = LocalNode::start(&owner_address.to_bech32(NETWORK), GENESIS_SPARKS).await;
    let chain = Chain::connect(&node.url()).expect("a client");
    let (vouchers, openings) = fund(&node, &chain, &parties, &mut rng).await;

    let attacker = Account::from_seed(&keys::random_bytes(&mut rng), NETWORK, 0).expect("account");
    let dir = tempfile::tempdir().expect("a temporary directory");
    let journal_path = dir.path().join("journal.jsonl");
    let context = Context {
        chain: chain.clone(),
        adversary: Adversary {
            delegate_key: parties.delegate_key,
            address: attacker.address_at(util::RECEIVING, 0).expect("address"),
        },
        vouchers: vouchers.clone(),
        journal: Some(journal_path.clone()),
    };

    let refused: Vec<Attack> = ALL
        .into_iter()
        .filter(|attack| !attack.is_valid_payment() && *attack != Attack::RedeemSpent)
        .collect();
    for attack in &refused {
        let report = run::attempt(&context, *attack, None)
            .await
            .expect("the attempt runs");
        println!("{}: {:?}", attack.name(), report.verdict);
        assert!(
            report.as_expected(),
            "{}: {:?}",
            attack.name(),
            report.verdict
        );
        assert_eq!(report.before, "unspent");
        assert_eq!(
            report.after,
            "unspent",
            "{} touched the voucher",
            attack.name()
        );
        let stage_ok = match attack {
            Attack::ForgedLeaf
            | Attack::StrippedLeaf
            | Attack::ExtraArgument
            | Attack::FeeSiphon => {
                matches!(report.verdict, Verdict::Prover(_))
            }
            Attack::KeyPath | Attack::OwnerBranch | Attack::MissingSignature => matches!(
                report.verdict,
                Verdict::Node {
                    verifier: Some(_),
                    ..
                }
            ),
            _ => matches!(report.verdict, Verdict::Node { .. }),
        };
        assert!(stage_ok, "{}: {:?}", attack.name(), report.verdict);
        node.mint();
    }

    // The owner recovers a voucher; redeeming it is then refused by the node.
    let target = &vouchers[1];
    let back = parties.owner.address_at(util::CHANGE, 1).expect("address");
    let prepared = prepare_output(&output(back, target.qty), &mut rng).expect("prepare");
    let unsigned = build::recovery(
        &[(target, &openings[1])],
        back.spending_key().compress(),
        &prepared,
        0,
    )
    .expect("build");
    let txid = unsigned.log().txid();
    let tx = build::sign(unsigned, &[parties.owner_key]).expect("sign");
    let proofs = chain.fresh_proofs(&[target.id()]).await.expect("a proof");
    chain
        .submit(build::package(tx, proofs).expect("package"))
        .await
        .expect("the recovery is admitted");
    node.mint();
    chain
        .wait_confirmed(&txid, PATIENCE)
        .await
        .expect("confirmed");

    let report = run::attempt(&context, Attack::RedeemSpent, None)
        .await
        .expect("the attempt runs");
    println!("redeem-spent: {:?}", report.verdict);
    assert_eq!(report.target, target.id());
    assert!(report.as_expected(), "{:?}", report.verdict);
    assert!(matches!(
        report.verdict,
        Verdict::Node { verifier: None, .. }
    ));
    let states = chain.states(&[target.id()]).await.expect("states");
    assert!(matches!(states[0], ContractState::Spent { txid: spent, .. } if spent == txid));

    // Every attempt is in the public journal, with the stage that stopped it.
    let journal = journal::read_all(&journal_path).expect("the journal");
    assert_eq!(journal.skipped, 0);
    assert_eq!(journal.entries.len(), refused.len() + 1);
    for entry in &journal.entries {
        assert_eq!(entry.actor, Actor::Attacker);
        assert!(matches!(entry.action, Action::Attack(_)));
        assert!(entry.error.is_some());
        assert!(matches!(entry.stage, Stage::Prover | Stage::Node));
        assert_eq!(entry.tx.is_some(), entry.stage == Stage::Node);
    }

    drop(context);
    drop(chain);
    node.stop().await;
}
