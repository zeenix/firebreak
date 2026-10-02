//! The agent's status: its allowances, and what every voucher is doing.

use std::fmt;

use firebreak_core::NETWORK;
use firebreak_core::store::{AgentStore, VoucherStatus, serde_address, serde_amount};
use flamekd::ReceivingAddress;
use serde::{Deserialize, Serialize};

use crate::{Agent, Error, Turn};

impl Agent {
    /// Reconciles the vouchers with the node, and reports the agent's allowances.
    ///
    /// A node that cannot be asked is not an error here: the states reported are the ones last
    /// recorded, the tip height is unknown, and the status says why. A problem with the agent's
    /// own files is an error.
    pub async fn status(&self) -> Result<Status, Error> {
        let turn = self.turn().await?;
        let (tip_height, warning) = match self.ask_node(&turn).await {
            Ok(height) => (Some(height), None),
            Err(Error::Chain(error)) => {
                let warning = format!(
                    "the node could not be asked, so the states are as last recorded: {error}"
                );
                (None, Some(warning))
            }
            Err(error) => return Err(error),
        };
        let records = self.files().read().await?;
        Ok(Status::new(&records, tip_height, warning))
    }

    /// Reconciles, and gives the height of the node's tip.
    async fn ask_node(&self, turn: &Turn) -> Result<u64, Error> {
        self.reconcile(turn).await?;
        Ok(self.chain().tip_height().await?)
    }
}

/// What the agent reports about itself: the JSON of `GET /api/status` and of `status --json`.
///
/// ```json
/// {"delegate": "<hex verification key>", "tip_height": 12,
///  "allowances": [{"allowance": "<id>", "merchant": "tf1...", "total": "100",
///                  "vouchers": [{"id": "<hex>", "qty": "50", "state": "unspent",
///                                "txid": "<hex or null>"}]}]}
/// ```
///
/// Amounts are decimal strings. A `warning` is added when the node could not be asked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// The delegated verification key as 64 hex digits. The secret key is never part of a status.
    pub delegate: String,
    /// The allowances imported so far.
    pub allowances: Vec<AllowanceStatus>,
    /// The height of the node's tip, or `null` when the node could not be asked.
    pub tip_height: Option<u64>,
    /// Why the voucher states may be out of date, when the node could not be asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// One allowance in a [`Status`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowanceStatus {
    /// The allowance's id: the first 16 hex digits of its funding transaction's id.
    pub allowance: String,
    /// The merchant every voucher of the allowance pays.
    #[serde(with = "serde_address")]
    pub merchant: ReceivingAddress,
    /// What the vouchers are worth together, in sparks.
    #[serde(with = "serde_amount")]
    pub total: u64,
    /// The vouchers, with the state the agent last recorded.
    pub vouchers: Vec<VoucherStatus>,
}

impl Status {
    /// The status of the agent whose records are `records`.
    pub fn new(records: &AgentStore, tip_height: Option<u64>, warning: Option<String>) -> Status {
        let allowances = records
            .allowances
            .iter()
            .map(|allowance| AllowanceStatus {
                allowance: allowance.allowance.clone(),
                merchant: allowance.merchant,
                total: sum(allowance.vouchers.iter().map(|voucher| voucher.qty)),
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
            })
            .collect();
        Status {
            delegate: hex::encode(records.public().to_bytes()),
            allowances,
            tip_height,
            warning,
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "delegate key (public): {}", self.delegate)?;
        match self.tip_height {
            Some(height) => writeln!(formatter, "node tip height: {height}")?,
            None => writeln!(formatter, "node tip height: unknown")?,
        }
        if let Some(warning) = &self.warning {
            writeln!(formatter, "warning: {warning}")?;
        }
        if self.allowances.is_empty() {
            return writeln!(formatter, "\nno allowance is imported yet");
        }
        for allowance in &self.allowances {
            writeln!(
                formatter,
                "\nallowance {}: {} sparks for merchant {}",
                allowance.allowance,
                allowance.total,
                allowance.merchant.to_bech32(NETWORK)
            )?;
            for voucher in &allowance.vouchers {
                write!(
                    formatter,
                    "  {:>10} sparks  {}  {}",
                    voucher.qty,
                    hex::encode(voucher.id),
                    voucher.state
                )?;
                if let Some(txid) = voucher.txid {
                    write!(formatter, "  redemption {}", hex::encode(txid.0))?;
                }
                writeln!(formatter)?;
            }
        }
        Ok(())
    }
}

/// The sum of `quantities`, which stops at the largest amount instead of overflowing.
pub(crate) fn sum<I>(quantities: I) -> u64
where
    I: IntoIterator<Item = u64>,
{
    quantities.into_iter().fold(0, u64::saturating_add)
}
