//! What the node says about the owner's wallet and vouchers, reconciled with what the owner
//! recorded.

use std::collections::HashMap;
use std::fmt;
use std::iter;

use firebreak_core::chain::{Chain, ContractState, TxState};
use firebreak_core::store::{
    self, AllowanceStatus, Observed, OutputStatus, OwnerAllowance, OwnerStatus, OwnerStore,
    Progress, RecoveryStatus, Role, VoucherState, VoucherStatus, WalletStatus, reconcile,
};
use firebreak_core::wallet::{self, Skipped, Synced};

use crate::{Context, Error, Files};

/// The owner's state as the node confirms it.
#[derive(Debug)]
pub struct StatusReport {
    /// The snapshot, exactly as written to `owner-status.json`.
    pub status: OwnerStatus,
    /// The outputs under the wallet's addresses that the wallet cannot use.
    pub skipped: Vec<Skipped>,
}

impl fmt::Display for StatusReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status = &self.status;
        let mut lines = vec![format!(
            "wallet: {} sparks (chain height {})",
            status.wallet.balance, status.tip_height
        )];
        for output in &status.wallet.outputs {
            lines.push(format!(
                "  output {}  {} sparks  {}",
                hex::encode(output.id),
                output.qty,
                output.state
            ));
        }
        for skipped in &self.skipped {
            lines.push(format!("  warning: {skipped}"));
        }
        if status.allowances.is_empty() {
            lines.push("no allowances".to_owned());
        }
        for allowance in &status.allowances {
            lines.push(format!(
                "allowance {}: {} sparks for {}",
                allowance.allowance,
                allowance.total,
                allowance.merchant.to_bech32(firebreak_core::NETWORK)
            ));
            lines.push(format!(
                "  delegate {}  funding transaction {}",
                hex::encode(allowance.delegate.to_bytes()),
                hex::encode(allowance.funding_txid.0)
            ));
            for voucher in &allowance.vouchers {
                lines.push(format!(
                    "  voucher {}  {} sparks  {}",
                    hex::encode(voucher.id),
                    voucher.qty,
                    voucher.state
                ));
            }
            for recovery in &allowance.recovery {
                lines.push(format!(
                    "  recovery {}: {} voucher(s), {}",
                    hex::encode(recovery.txid.0),
                    recovery.vouchers.len(),
                    match recovery.state {
                        Progress::Pending => "recovery pending",
                        Progress::Confirmed => "confirmed",
                    }
                ));
            }
        }
        write!(formatter, "{}", lines.join("\n"))
    }
}

/// Reconciles every voucher with the node, finds the wallet's outputs, and writes the snapshot
/// the dashboard reads.
///
/// When the node does not answer, nothing is changed, the snapshot is left as it was, and the
/// error says the outcome is unknown.
pub async fn status(ctx: &Context) -> Result<StatusReport, Error> {
    refresh(ctx).await
}

/// What [`status`] does, for the commands that bring the snapshot up to date when they finish.
pub(crate) async fn refresh(ctx: &Context) -> Result<StatusReport, Error> {
    let store = ctx.files.load()?;
    let tip_height = ctx.chain.tip_height().await.map_err(Error::node)?;
    let observation = observe(&ctx.chain, &store).await?;
    let account = store.account().map_err(Error::Wallet)?;
    let synced = wallet::sync(
        &ctx.chain,
        &account,
        0..account.next_index(),
        store.change_range(),
    )
    .await
    .map_err(Error::sync)?;

    let store = ctx.files.update(|store| {
        apply(store, &observation);
        Ok(store.clone())
    })?;
    restore_packages(&ctx.files, &store)?;
    let status = snapshot(tip_height, &synced, &store);
    store::write_public(&ctx.files.status(), &status)?;
    Ok(StatusReport {
        status,
        skipped: synced.skipped,
    })
}

/// What the node said about the owner's vouchers and transactions at one time.
struct Observation {
    /// Each voucher's contract, by contract id.
    contracts: HashMap<[u8; 32], ContractState>,
    /// Where each funding and recovery transaction is, by transaction id.
    transactions: HashMap<[u8; 32], TxState>,
}

/// Asks the node about every voucher and every transaction in `store`.
///
/// The transactions are asked about first. A transaction only ever moves from the mempool into a
/// block and a contract only ever from unspent to spent, so a contract that is asked about
/// afterwards is never older than its transaction: the spend of a transaction found confirmed is
/// seen, and a spend never shows up whose transaction was last seen waiting.
async fn observe(chain: &Chain, store: &OwnerStore) -> Result<Observation, Error> {
    let mut transactions = HashMap::new();
    for allowance in &store.allowances {
        let recoveries = allowance.recoveries.iter().map(|recovery| recovery.txid);
        for txid in iter::once(allowance.funding_txid).chain(recoveries) {
            if transactions.contains_key(&txid.0) {
                continue;
            }
            let state = chain.tx_state(&txid).await.map_err(Error::node)?;
            transactions.insert(txid.0, state);
        }
    }

    let ids: Vec<[u8; 32]> = store
        .allowances
        .iter()
        .flat_map(|allowance| allowance.vouchers.iter().map(|voucher| voucher.id))
        .collect();
    let states = chain.states(&ids).await.map_err(Error::node)?;
    Ok(Observation {
        contracts: ids.into_iter().zip(states).collect(),
        transactions,
    })
}

/// Brings the states in `store` in line with `observation`.
///
/// The store is the one under the lock, which may have changed since the node was asked: an
/// allowance, or a recovery of one, that the observation does not cover is left for the next
/// reconciliation. A recovery that the node does not know, and that never confirmed, was refused
/// or dropped, so it recovers nothing and is forgotten; the vouchers it named are unspent again.
fn apply(store: &mut OwnerStore, observation: &Observation) {
    for allowance in &mut store.allowances {
        let Some(funding) = observation
            .transactions
            .get(&allowance.funding_txid.0)
            .copied()
        else {
            continue;
        };
        let states: Option<Vec<TxState>> = allowance
            .recoveries
            .iter()
            .map(|recovery| observation.transactions.get(&recovery.txid.0).copied())
            .collect();
        let Some(states) = states else {
            continue;
        };

        for voucher in &mut allowance.vouchers {
            let Some(contract) = observation.contracts.get(&voucher.id) else {
                continue;
            };
            // The voucher's latest recovery is the one whose fate matters to it.
            let own_spend = allowance
                .recoveries
                .iter()
                .zip(&states)
                .rev()
                .find(|(recovery, _)| recovery.vouchers.contains(&voucher.id))
                .map(|(recovery, state)| (recovery.txid, *state));
            let observed = Observed {
                contract: contract.clone(),
                funding,
                own_spend,
            };
            voucher.state = reconcile(Role::Owner, voucher.state, &observed);
            voucher.txid = match (contract, voucher.state) {
                (ContractState::Spent { txid, .. }, _) => Some(*txid),
                (_, VoucherState::RecoveryPending) => own_spend.map(|(txid, _)| txid),
                _ => None,
            };
        }

        let mut states = states.into_iter();
        allowance.recoveries.retain_mut(|recovery| {
            match states.next() {
                Some(TxState::Confirmed { .. }) => recovery.state = Progress::Confirmed,
                Some(TxState::Mempool) => recovery.state = Progress::Pending,
                Some(TxState::Unknown) | None => return recovery.state == Progress::Confirmed,
            }
            true
        });
    }
}

/// Writes the delegation package of every allowance that was offered to the node and has none.
///
/// Funding writes the package as soon as the node has answered. If the program stopped between
/// the answer and the write, the allowance is on the node's side and the delegate cannot pay from
/// it without its package.
fn restore_packages(files: &Files, store: &OwnerStore) -> Result<(), Error> {
    for allowance in &store.allowances {
        let offered = allowance
            .vouchers
            .iter()
            .any(|voucher| voucher.state != VoucherState::Prepared);
        let path = files.package(&allowance.allowance);
        if offered && !path.exists() {
            let package = allowance.package().map_err(Error::Wallet)?;
            store::write_public(&path, &package)?;
        }
    }
    Ok(())
}

/// The snapshot of `store` and the wallet outputs `synced` found, at chain height `tip_height`.
fn snapshot(tip_height: u64, synced: &Synced, store: &OwnerStore) -> OwnerStatus {
    let outputs: Vec<OutputStatus> = synced
        .outputs
        .iter()
        .filter(|output| output.spent.is_none())
        .map(|output| OutputStatus {
            id: output.id,
            qty: output.qty,
            state: VoucherState::Unspent,
        })
        .collect();
    let balance = outputs
        .iter()
        .fold(0u64, |sum, output| sum.saturating_add(output.qty));
    OwnerStatus {
        updated: store::unix_now(),
        tip_height,
        wallet: WalletStatus { balance, outputs },
        allowances: store.allowances.iter().map(allowance_status).collect(),
    }
}

/// An allowance as the snapshot shows it.
fn allowance_status(allowance: &OwnerAllowance) -> AllowanceStatus {
    AllowanceStatus {
        allowance: allowance.allowance.clone(),
        merchant: allowance.merchant,
        delegate: allowance.delegate,
        total: allowance.total(),
        funding_txid: allowance.funding_txid,
        vouchers: allowance
            .vouchers
            .iter()
            .map(|voucher| VoucherStatus {
                id: voucher.id,
                qty: voucher.qty,
                state: voucher.state,
                txid: voucher.txid,
            })
            .collect(),
        recovery: allowance
            .recoveries
            .iter()
            .map(|recovery| RecoveryStatus {
                txid: recovery.txid,
                vouchers: recovery.vouchers.clone(),
                state: recovery.state,
            })
            .collect(),
    }
}
