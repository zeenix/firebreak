//! Bringing the recorded voucher states in line with what the node says.

use std::collections::BTreeMap;

use firebreak_core::chain::TxState;
use firebreak_core::store::{self, AgentAllowance, AgentStore, AgentVoucher, Observed};
use firebreak_core::store::{Role, VoucherState};
use firebreak_core::{Chain, ChainError};
use flamevm::TxID;

use crate::{Agent, Error, Turn};

impl Agent {
    /// Asks the node about every voucher that is not settled yet, and records what it says.
    ///
    /// The node decides: a voucher is spent once a block holds a spend, `redeemed` when the spend
    /// is this agent's redemption and `recovered` when it is anyone else's. A voucher the agent
    /// reserved stays `redemption_pending` only while its transaction waits in the node's
    /// mempool. A voucher that is `redeemed` or `recovered` is final, so it is not asked about
    /// again, which also keeps the record of its redemption from ever being reinterpreted.
    ///
    /// When the node cannot be asked, no state changes: the agent never guesses. Asking needs the
    /// turn, which the caller proves by handing it in.
    pub(crate) async fn reconcile(&self, _turn: &Turn) -> Result<(), Error> {
        let records = self.files().read().await?;
        let changes = observe(self.chain(), &records).await?;
        self.files()
            .update(move |records| {
                apply(records, &changes);
                Ok(())
            })
            .await
    }
}

/// The state the node's answers give a voucher.
struct Change {
    allowance: String,
    voucher: [u8; 32],
    state: VoucherState,
}

/// What the node says about every voucher in `records` that is not settled.
///
/// The node is asked about the transactions first and about the contracts after, and the order
/// matters. A transaction only moves forward, from unknown to the mempool to a block, and so does
/// a contract, from unspent to spent. Asked in this order, a contract is never older than the
/// transactions seen with it. In the other order, a block that arrives in between shows a voucher
/// that is still unspent next to its own redemption already in a block, which looks exactly like
/// a reservation whose transaction was dropped.
async fn observe(chain: &Chain, records: &AgentStore) -> Result<Vec<Change>, ChainError> {
    let open: Vec<(&AgentAllowance, &AgentVoucher)> = records
        .allowances
        .iter()
        .flat_map(|allowance| {
            allowance
                .vouchers
                .iter()
                .filter(|voucher| !voucher.state.is_settled())
                .map(move |voucher| (allowance, voucher))
        })
        .collect();

    let mut transactions = Transactions {
        chain,
        known: BTreeMap::new(),
    };
    let mut asked = Vec::with_capacity(open.len());
    for (allowance, voucher) in &open {
        let funding = transactions.state(&allowance.funding_txid).await?;
        let own_spend = match voucher.txid {
            Some(txid) => Some((txid, transactions.state(&txid).await?)),
            None => None,
        };
        asked.push((funding, own_spend));
    }
    let ids: Vec<[u8; 32]> = open.iter().map(|(_, voucher)| voucher.id).collect();
    let contracts = chain.states(&ids).await?;

    let mut changes = Vec::with_capacity(open.len());
    for (((allowance, voucher), (funding, own_spend)), contract) in
        open.into_iter().zip(asked).zip(contracts)
    {
        let observed = Observed {
            contract,
            funding,
            own_spend,
        };
        changes.push(Change {
            allowance: allowance.allowance.clone(),
            voucher: voucher.id,
            state: store::reconcile(Role::Agent, voucher.state, &observed),
        });
    }
    Ok(changes)
}

/// Records `changes` in `records`.
///
/// The transaction of a voucher is kept while it is the one spending the voucher or the one that
/// spent it. A voucher that is `unspent` again lost its reservation, and one that was recovered
/// was never spent by the agent, so neither has a redemption to name.
fn apply(records: &mut AgentStore, changes: &[Change]) {
    for change in changes {
        let found = records
            .allowance_mut(&change.allowance)
            .and_then(|allowance| {
                allowance
                    .vouchers
                    .iter_mut()
                    .find(|voucher| voucher.id == change.voucher)
            });
        let Some(voucher) = found else {
            continue;
        };
        voucher.state = change.state;
        let redeeming = matches!(
            change.state,
            VoucherState::RedemptionPending | VoucherState::Redeemed
        );
        if !redeeming {
            voucher.txid = None;
        }
    }
}

/// Where transactions are, with each one asked about once.
struct Transactions<'a> {
    chain: &'a Chain,
    known: BTreeMap<[u8; 32], TxState>,
}

impl Transactions<'_> {
    async fn state(&mut self, txid: &TxID) -> Result<TxState, ChainError> {
        if let Some(state) = self.known.get(&txid.0) {
            return Ok(*state);
        }
        let state = self.chain.tx_state(txid).await?;
        self.known.insert(txid.0, state);
        Ok(state)
    }
}
