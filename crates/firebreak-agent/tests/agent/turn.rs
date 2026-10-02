//! The turn: a reconciliation never meets a payment halfway.

use std::time::Duration;

use firebreak_agent::Agent;
use firebreak_core::store::VoucherState;
use flamevm::TxID;

use crate::fixture::World;

/// How long a task that must wait is given to prove that it waits.
const GRACE: Duration = Duration::from_millis(300);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reconciliation_waits_for_a_payment_in_flight_and_then_frees_what_never_went_out() {
    let world = World::start_manual().await;
    // The agent of this process, and another handle on the same files and node, which stands for
    // the command line while the server runs.
    let other = Agent::new(world.files.clone(), world.net.chain.clone());

    for waiter in [&*world.agent, &other] {
        // A payment has reserved its vouchers for a transaction that the node has not seen yet.
        let turn = world.agent.turn().await.expect("the turn");
        let in_flight = TxID([7; 32]);
        world
            .files
            .update(move |records| {
                for voucher in &mut records.allowances[0].vouchers {
                    if voucher.qty == 50 || voucher.qty == 10 {
                        voucher.state = VoucherState::RedemptionPending;
                        voucher.txid = Some(in_flight);
                    }
                }
                Ok(())
            })
            .await
            .expect("reserve");

        // A reconciliation now would take the reservation for a dropped transaction. It waits.
        let status = tokio::time::timeout(GRACE, waiter.status()).await;
        assert!(status.is_err(), "the status did not wait for the turn");
        let held = world.recorded().await;
        assert_eq!(
            held[0],
            (50, VoucherState::RedemptionPending, Some(in_flight))
        );
        assert_eq!(
            held[3],
            (10, VoucherState::RedemptionPending, Some(in_flight))
        );

        // The payment ends without ever submitting, as one that crashed does. The next
        // reconciliation finds that the node does not know the transaction, and frees the
        // vouchers.
        drop(turn);
        waiter.status().await.expect("a status");
        let freed = world.recorded().await;
        assert!(
            freed
                .iter()
                .all(|(_, state, txid)| *state == VoucherState::Unspent && txid.is_none()),
            "{freed:?}"
        );
    }
}
