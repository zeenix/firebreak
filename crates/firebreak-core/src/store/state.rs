//! The states a voucher moves through, and how far a submitted transaction has come.

use std::fmt;

use flamevm::TxID;
use serde::{Deserialize, Serialize};

use crate::chain::{ContractState, TxState};

/// Where a voucher stands, as the role that wrote the word last learned it.
///
/// Every role uses the same eight states, written in snake case in JSON. A voucher is only ever
/// called `redeemed` or `recovered` once the transaction that spent it confirmed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoucherState {
    /// The owner holds everything needed to recover the voucher, and has not yet submitted the
    /// transaction that funds it.
    Prepared,
    /// The funding transaction was submitted and has not confirmed.
    FundingPending,
    /// The funding confirmed and nothing has spent the voucher.
    Unspent,
    /// A redemption was submitted and has not confirmed.
    RedemptionPending,
    /// A recovery was submitted and has not confirmed.
    RecoveryPending,
    /// A confirmed redemption spent the voucher, which paid the merchant.
    Redeemed,
    /// A confirmed recovery spent the voucher, which paid the owner.
    Recovered,
    /// What is known locally is not enough to say; resynchronize before acting.
    Unknown,
}

impl VoucherState {
    /// Every state, in the order of a voucher's life.
    pub const ALL: [VoucherState; 8] = [
        VoucherState::Prepared,
        VoucherState::FundingPending,
        VoucherState::Unspent,
        VoucherState::RedemptionPending,
        VoucherState::RecoveryPending,
        VoucherState::Redeemed,
        VoucherState::Recovered,
        VoucherState::Unknown,
    ];

    /// The state's name as written in JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            VoucherState::Prepared => "prepared",
            VoucherState::FundingPending => "funding_pending",
            VoucherState::Unspent => "unspent",
            VoucherState::RedemptionPending => "redemption_pending",
            VoucherState::RecoveryPending => "recovery_pending",
            VoucherState::Redeemed => "redeemed",
            VoucherState::Recovered => "recovered",
            VoucherState::Unknown => "unknown",
        }
    }

    /// Whether a transaction for the voucher was submitted and has not confirmed.
    pub fn is_pending(self) -> bool {
        matches!(
            self,
            VoucherState::FundingPending
                | VoucherState::RedemptionPending
                | VoucherState::RecoveryPending
        )
    }

    /// Whether a confirmed transaction spent the voucher, so that it is finished.
    pub fn is_settled(self) -> bool {
        matches!(self, VoucherState::Redeemed | VoucherState::Recovered)
    }
}

impl fmt::Display for VoucherState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// How far a transaction a role submitted has come.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Progress {
    /// Submitted, and not yet in a block.
    Pending,
    /// In a block.
    Confirmed,
}

/// Which side of a voucher a role is on, and so which spends it recognizes as its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The owner, which funds vouchers and recovers them.
    Owner,
    /// The delegate, which redeems them.
    Agent,
}

/// What the node reports now about one voucher, and about the transactions a role made for it.
#[derive(Clone, Debug)]
pub struct Observed {
    /// The voucher's contract.
    pub contract: ContractState,
    /// The transaction that funds the voucher.
    pub funding: TxState,
    /// The role's own spend of the voucher, when it submitted one: the owner's recovery or the
    /// agent's redemption, with that transaction's state.
    pub own_spend: Option<(TxID, TxState)>,
}

/// The state of a voucher, given the state a role recorded last and what the node reports now.
///
/// The node alone decides that a voucher is spent, and it reports a spend only once a block holds
/// it, so only a confirmed spend makes a voucher `redeemed` or `recovered`. A spend the role did
/// not make is the other branch's: the owner sees a redemption, the agent a recovery. A pending
/// state survives while the node knows its transaction, in the mempool or in a block that the
/// contract's observation predates; once the node no longer knows the transaction, it was dropped
/// and the voucher is whatever the chain says. A funding the node does not know leaves the
/// allowance `prepared`, which its owner may submit again: rebuilt from the same inputs and
/// outputs, it has the same transaction id and creates the same vouchers.
pub fn reconcile(role: Role, recorded: VoucherState, observed: &Observed) -> VoucherState {
    match &observed.contract {
        ContractState::Spent { txid, .. } => {
            let own = observed.own_spend.is_some_and(|(own, _)| own == *txid);
            match (role, own) {
                (Role::Owner, true) | (Role::Agent, false) => VoucherState::Recovered,
                (Role::Owner, false) | (Role::Agent, true) => VoucherState::Redeemed,
            }
        }
        ContractState::Unspent(_) => {
            let pending = match role {
                Role::Owner => VoucherState::RecoveryPending,
                Role::Agent => VoucherState::RedemptionPending,
            };
            // A spend the node reports confirmed while the contract still reads unspent means the
            // contract was observed before the block that holds the spend: the next observation
            // shows it spent. Only a spend the node no longer knows at all was dropped.
            let alive = matches!(
                observed.own_spend,
                Some((_, TxState::Mempool | TxState::Confirmed { .. }))
            );
            if recorded == pending && alive {
                pending
            } else {
                VoucherState::Unspent
            }
        }
        ContractState::Unknown => match observed.funding {
            TxState::Mempool => VoucherState::FundingPending,
            TxState::Unknown
                if matches!(
                    recorded,
                    VoucherState::Prepared | VoucherState::FundingPending
                ) =>
            {
                VoucherState::Prepared
            }
            TxState::Unknown | TxState::Confirmed { .. } => VoucherState::Unknown,
        },
    }
}

#[cfg(test)]
mod tests {
    use flamechain::utreexo::Proof;

    use super::*;

    const OURS: TxID = TxID([1; 32]);
    const THEIRS: TxID = TxID([2; 32]);

    fn observed(
        contract: ContractState,
        funding: TxState,
        own_spend: Option<(TxID, TxState)>,
    ) -> Observed {
        Observed {
            contract,
            funding,
            own_spend,
        }
    }

    fn spent(txid: TxID) -> ContractState {
        ContractState::Spent { height: 3, txid }
    }

    #[test]
    fn a_confirmed_spend_is_named_by_whose_it_is() {
        let confirmed = TxState::Confirmed { height: 3 };
        let cases = [
            (
                Role::Owner,
                Some(OURS),
                spent(OURS),
                VoucherState::Recovered,
            ),
            (
                Role::Owner,
                Some(OURS),
                spent(THEIRS),
                VoucherState::Redeemed,
            ),
            (Role::Owner, None, spent(THEIRS), VoucherState::Redeemed),
            (Role::Agent, Some(OURS), spent(OURS), VoucherState::Redeemed),
            (
                Role::Agent,
                Some(OURS),
                spent(THEIRS),
                VoucherState::Recovered,
            ),
            (Role::Agent, None, spent(THEIRS), VoucherState::Recovered),
        ];
        for (role, own, contract, expected) in cases {
            let own_spend = own.map(|txid| (txid, TxState::Unknown));
            let state = reconcile(
                role,
                VoucherState::Unspent,
                &observed(contract, confirmed, own_spend),
            );
            assert_eq!(state, expected, "{role:?} with {own:?}");
        }
    }

    #[test]
    fn a_pending_spend_lasts_only_while_its_transaction_waits() {
        let confirmed = TxState::Confirmed { height: 1 };
        let unspent = || ContractState::Unspent(Proof::Transient);
        for (role, pending) in [
            (Role::Owner, VoucherState::RecoveryPending),
            (Role::Agent, VoucherState::RedemptionPending),
        ] {
            let waiting = observed(unspent(), confirmed, Some((OURS, TxState::Mempool)));
            assert_eq!(reconcile(role, pending, &waiting), pending);
            // A block between the two queries: the spend is in it, the contract read predates it.
            let raced = observed(unspent(), confirmed, Some((OURS, confirmed)));
            assert_eq!(reconcile(role, pending, &raced), pending);
            let dropped = observed(unspent(), confirmed, Some((OURS, TxState::Unknown)));
            assert_eq!(reconcile(role, pending, &dropped), VoucherState::Unspent);
            let nothing = observed(unspent(), confirmed, None);
            assert_eq!(reconcile(role, pending, &nothing), VoucherState::Unspent);
        }
        // The other side's pending state is not this role's to keep.
        let waiting = observed(unspent(), confirmed, Some((OURS, TxState::Mempool)));
        assert_eq!(
            reconcile(Role::Owner, VoucherState::RedemptionPending, &waiting),
            VoucherState::Unspent
        );
    }

    #[test]
    fn an_unknown_voucher_follows_its_funding() {
        let unknown = || ContractState::Unknown;
        assert_eq!(
            reconcile(
                Role::Owner,
                VoucherState::Prepared,
                &observed(unknown(), TxState::Mempool, None)
            ),
            VoucherState::FundingPending
        );
        for recorded in [VoucherState::Prepared, VoucherState::FundingPending] {
            assert_eq!(
                reconcile(
                    Role::Owner,
                    recorded,
                    &observed(unknown(), TxState::Unknown, None)
                ),
                VoucherState::Prepared
            );
        }
        assert_eq!(
            reconcile(
                Role::Agent,
                VoucherState::Unspent,
                &observed(unknown(), TxState::Unknown, None)
            ),
            VoucherState::Unknown
        );
        assert_eq!(
            reconcile(
                Role::Agent,
                VoucherState::Unspent,
                &observed(unknown(), TxState::Confirmed { height: 1 }, None)
            ),
            VoucherState::Unknown
        );
    }

    #[test]
    fn a_state_is_written_in_snake_case() {
        for (state, name) in VoucherState::ALL.into_iter().zip([
            "prepared",
            "funding_pending",
            "unspent",
            "redemption_pending",
            "recovery_pending",
            "redeemed",
            "recovered",
            "unknown",
        ]) {
            assert_eq!(state.as_str(), name);
            assert_eq!(state.to_string(), name);
            assert_eq!(serde_json::to_value(state).expect("serialize"), name);
            let back: VoucherState = serde_json::from_value(name.into()).expect("deserialize");
            assert_eq!(back, state);
        }
    }

    #[test]
    fn a_state_the_spec_does_not_name_is_refused() {
        for name in ["spent", "Unspent", "funding-pending", "spent_elsewhere", ""] {
            assert!(
                serde_json::from_value::<VoucherState>(name.into()).is_err(),
                "{name}"
            );
        }
    }

    #[test]
    fn pending_and_settled_states_are_told_apart() {
        let pending: Vec<VoucherState> = VoucherState::ALL
            .into_iter()
            .filter(|state| state.is_pending())
            .collect();
        assert_eq!(
            pending,
            [
                VoucherState::FundingPending,
                VoucherState::RedemptionPending,
                VoucherState::RecoveryPending
            ]
        );
        let settled: Vec<VoucherState> = VoucherState::ALL
            .into_iter()
            .filter(|state| state.is_settled())
            .collect();
        assert_eq!(settled, [VoucherState::Redeemed, VoucherState::Recovered]);
    }

    #[test]
    fn progress_is_pending_or_confirmed() {
        assert_eq!(
            serde_json::to_value(Progress::Pending).expect("serialize"),
            "pending"
        );
        assert_eq!(
            serde_json::to_value(Progress::Confirmed).expect("serialize"),
            "confirmed"
        );
        assert!(serde_json::from_value::<Progress>("done".into()).is_err());
    }
}
