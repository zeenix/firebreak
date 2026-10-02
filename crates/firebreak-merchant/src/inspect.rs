//! What the node says the merchant has received and spent.
//!
//! A payment is an output under the merchant's published address, with the receipt that
//! the redemption logged right after it. The receipt is a note sealed to the merchant's viewing
//! key: opening it gives the amount, checked against the token's commitments, and the memo.

use std::collections::HashMap;
use std::fmt;

use firebreak_core::NETWORK;
use firebreak_core::chain::TxState;
use firebreak_core::store::{self, MerchantStatus, MerchantStore, Progress, Receipt};
use firebreak_core::wallet::{self, Skipped, Synced};
use flamekd::util;

use crate::{Context, Error};

/// The merchant's state as the node confirms it.
#[derive(Debug)]
pub struct InspectReport {
    /// The snapshot, exactly as written to `merchant-status.json`.
    pub status: MerchantStatus,
    /// The outputs under the merchant's addresses that it cannot use, such as a payment whose
    /// receipt does not open.
    pub skipped: Vec<Skipped>,
}

impl fmt::Display for InspectReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status = &self.status;
        let mut lines = vec![
            format!("merchant address {}", status.address.to_bech32(NETWORK)),
            format!(
                "balance: {} sparks (chain height {})",
                status.balance, status.tip_height
            ),
        ];
        if status.receipts.is_empty() {
            lines.push("no payments received yet".to_owned());
        }
        for receipt in &status.receipts {
            let fate = match receipt.spent_txid {
                Some(txid) => format!("spent by {}", hex::encode(txid.0)),
                None => "unspent".to_owned(),
            };
            lines.push(format!(
                "receipt {}  {} sparks  transaction {} in block {}  {:?}  {fate}",
                hex::encode(receipt.id),
                receipt.qty,
                hex::encode(receipt.txid.0),
                receipt.height,
                receipt.memo
            ));
        }
        for spend in &status.spends {
            lines.push(format!(
                "spend {}  {} sparks to {}  {}",
                hex::encode(spend.txid.0),
                spend.qty,
                spend.to.to_bech32(NETWORK),
                match spend.state {
                    Progress::Pending => "pending",
                    Progress::Confirmed => "confirmed",
                }
            ));
        }
        for skipped in &self.skipped {
            lines.push(format!("warning: {skipped}"));
        }
        write!(formatter, "{}", lines.join("\n"))
    }
}

/// Finds the payments the merchant has received, opens their receipts, and writes the snapshot
/// the dashboard reads.
///
/// An output that cannot be opened is reported and left out, and never makes the command fail.
/// When the node does not answer, nothing is changed, the snapshot is left as it was, and the
/// error says the outcome is unknown.
pub async fn inspect(ctx: &Context) -> Result<InspectReport, Error> {
    let store = ctx.files.load()?;
    let observation = observe(ctx, &store).await?;
    settle(ctx, &observation)
}

/// What the node said about the merchant's wallet and spends at one time.
pub(crate) struct Observation {
    /// The height of the chain's tip.
    pub tip_height: u64,
    /// The outputs under the merchant's issued addresses, spent and unspent.
    pub synced: Synced,
    /// Where each spend the merchant recorded is, by transaction id.
    spends: HashMap<[u8; 32], TxState>,
}

/// Asks the node about the merchant's spends and then its outputs.
///
/// The spends come first. A transaction only ever moves from the mempool into a block and an
/// output only ever from unspent to spent, so the outputs are never older than the state of the
/// transaction that spent them.
pub(crate) async fn observe(ctx: &Context, store: &MerchantStore) -> Result<Observation, Error> {
    let tip_height = ctx.chain.tip_height().await.map_err(Error::node)?;
    let mut spends = HashMap::new();
    for spend in &store.spends {
        let state = ctx.chain.tx_state(&spend.txid).await.map_err(Error::node)?;
        spends.insert(spend.txid.0, state);
    }
    let account = store.account().map_err(Error::Wallet)?;
    let synced = wallet::sync(&ctx.chain, &account, store.receiving_range(), 0..0)
        .await
        .map_err(Error::sync)?;
    Ok(Observation {
        tip_height,
        synced,
        spends,
    })
}

/// Records what `observation` found in the store and writes the snapshot.
pub(crate) fn settle(ctx: &Context, observation: &Observation) -> Result<InspectReport, Error> {
    let store = ctx.files.update(|store| {
        apply(store, observation);
        Ok(store.clone())
    })?;
    let status = snapshot(&store, observation)?;
    store::write_public(&ctx.files.status(), &status)?;
    Ok(InspectReport {
        status,
        skipped: observation.synced.skipped.clone(),
    })
}

/// Whether `output` is a payment to the merchant's published address, which is the first
/// receiving address. The merchant's other addresses only ever receive its own spends.
pub(crate) fn is_receipt(output: &wallet::OwnedOutput) -> bool {
    output.branch == util::RECEIVING && output.index == 0
}

/// Brings `store` in line with `observation`.
///
/// The store is the one under the lock, which may have changed since the node was asked: a spend
/// added after that is left for the next inspection. A spend that the node does not know, and
/// that never confirmed, was refused or dropped, so it moved nothing and is forgotten; the
/// receipts it named are unspent again.
fn apply(store: &mut MerchantStore, observation: &Observation) {
    store.receipts = observation
        .synced
        .outputs
        .iter()
        .filter(|output| is_receipt(output))
        .map(|output| Receipt {
            id: output.id,
            qty: output.qty,
            txid: output.txid,
            height: output.height,
            memo: String::from_utf8_lossy(&output.memo).into_owned(),
            spent: output.spent.is_some(),
            spent_txid: output.spent.map(|(_, txid)| txid),
        })
        .collect();
    store
        .spends
        .retain_mut(|spend| match observation.spends.get(&spend.txid.0) {
            Some(TxState::Confirmed { .. }) => {
                spend.state = Progress::Confirmed;
                true
            }
            Some(TxState::Mempool) => {
                spend.state = Progress::Pending;
                true
            }
            Some(TxState::Unknown) => spend.state == Progress::Confirmed,
            None => true,
        });
}

/// The snapshot of `store`, with the balance of the outputs `observation` found unspent.
fn snapshot(store: &MerchantStore, observation: &Observation) -> Result<MerchantStatus, Error> {
    let balance = observation
        .synced
        .outputs
        .iter()
        .filter(|output| output.spent.is_none())
        .fold(0u64, |sum, output| sum.saturating_add(output.qty));
    Ok(MerchantStatus {
        updated: store::unix_now(),
        tip_height: observation.tip_height,
        address: store.address().map_err(Error::Wallet)?,
        receipts: store.receipts.clone(),
        balance,
        spends: store.spends.clone(),
    })
}
