//! The snapshots the roles write for the dashboard: `owner-status.json` and
//! `merchant-status.json`.
//!
//! A snapshot is a point-in-time view that a role writes after it has reconciled with the node.
//! It holds no key, no opening and no note. The merchant's snapshot is the merchant's own view of
//! its receipts, shown in its panel only. Times are Unix seconds, and amounts are decimal strings
//! of sparks.

use curve25519_dalek::ristretto::CompressedRistretto;
use flamekd::ReceivingAddress;
use flamevm::TxID;
use serde::{Deserialize, Serialize};

use super::{Progress, Receipt, Spend, VoucherState, serde_address, serde_amount, serde_hex};

/// The owner's snapshot.
///
/// ```json
/// {
///   "updated": 1696000000,
///   "tip_height": 12,
///   "wallet": {"balance": "900", "outputs": [{"id": "<hex>", "qty": "900", "state": "unspent"}]},
///   "allowances": [
///     {"allowance": "<id>", "merchant": "tf1...", "delegate": "<hex>", "total": "100",
///      "funding_txid": "<hex>",
///      "vouchers": [{"id": "<hex>", "qty": "50", "state": "redeemed", "txid": "<hex or null>"}],
///      "recovery": [{"txid": "<hex>", "vouchers": ["<id>"], "state": "pending|confirmed"}]}
///   ]
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerStatus {
    /// When the snapshot was taken, in Unix seconds.
    pub updated: u64,
    /// The height of the chain's tip at that time.
    pub tip_height: u64,
    /// The owner's wallet.
    pub wallet: WalletStatus,
    /// The owner's allowances.
    pub allowances: Vec<AllowanceStatus>,
}

/// The owner's wallet in a snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalletStatus {
    /// What the wallet's spendable outputs are worth together, in sparks.
    #[serde(with = "serde_amount")]
    pub balance: u64,
    /// The wallet's outputs.
    pub outputs: Vec<OutputStatus>,
}

/// One wallet output in a snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputStatus {
    /// The output's contract id.
    #[serde(with = "serde_hex")]
    pub id: [u8; 32],
    /// The output's value in sparks.
    #[serde(with = "serde_amount")]
    pub qty: u64,
    /// Whether the output is spendable yet.
    pub state: VoucherState,
}

/// One allowance in the owner's snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowanceStatus {
    /// The allowance's id: the first 16 hex digits of its funding transaction's id.
    pub allowance: String,
    /// The merchant every voucher pays.
    #[serde(with = "serde_address")]
    pub merchant: ReceivingAddress,
    /// The key that redeems the vouchers.
    #[serde(with = "serde_hex")]
    pub delegate: CompressedRistretto,
    /// What the vouchers are worth together, in sparks.
    #[serde(with = "serde_amount")]
    pub total: u64,
    /// The transaction that funded the vouchers.
    #[serde(with = "serde_hex")]
    pub funding_txid: TxID,
    /// The vouchers.
    pub vouchers: Vec<VoucherStatus>,
    /// The recoveries the owner submitted.
    pub recovery: Vec<RecoveryStatus>,
}

/// One voucher in a snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoucherStatus {
    /// The voucher's contract id.
    #[serde(with = "serde_hex")]
    pub id: [u8; 32],
    /// The voucher's face value in sparks.
    #[serde(with = "serde_amount")]
    pub qty: u64,
    /// Where the voucher stands.
    pub state: VoucherState,
    /// The transaction that spent the voucher or is spending it, or `null`.
    #[serde(default, with = "serde_hex::option")]
    pub txid: Option<TxID>,
}

/// One recovery transaction in the owner's snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryStatus {
    /// The recovery transaction.
    #[serde(with = "serde_hex")]
    pub txid: TxID,
    /// The vouchers it recovers.
    #[serde(with = "serde_hex::vec")]
    pub vouchers: Vec<[u8; 32]>,
    /// Whether it confirmed.
    pub state: Progress,
}

/// The merchant's snapshot.
///
/// ```json
/// {"updated": 1696000000, "tip_height": 12, "address": "tf1...",
///  "receipts": [{"id": "<hex>", "qty": "50", "txid": "<hex>", "height": 7, "memo": "...",
///                "spent": false, "spent_txid": null}],
///  "balance": "60",
///  "spends": [{"txid": "<hex>", "qty": "60", "to": "tf1...", "state": "confirmed"}]}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MerchantStatus {
    /// When the snapshot was taken, in Unix seconds.
    pub updated: u64,
    /// The height of the chain's tip at that time.
    pub tip_height: u64,
    /// The address the merchant publishes.
    #[serde(with = "serde_address")]
    pub address: ReceivingAddress,
    /// The payments the merchant found.
    pub receipts: Vec<Receipt>,
    /// What the unspent receipts are worth together, in sparks.
    #[serde(with = "serde_amount")]
    pub balance: u64,
    /// The transactions that spent what the merchant received.
    pub spends: Vec<Spend>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::store::fixtures;

    fn hex32(byte: u8) -> String {
        hex::encode([byte; 32])
    }

    fn owner_status() -> OwnerStatus {
        OwnerStatus {
            updated: 1_696_000_000,
            tip_height: 12,
            wallet: WalletStatus {
                balance: 900,
                outputs: vec![OutputStatus {
                    id: [1; 32],
                    qty: 900,
                    state: VoucherState::Unspent,
                }],
            },
            allowances: vec![AllowanceStatus {
                allowance: "0202020202020202".to_owned(),
                merchant: fixtures::address(1),
                delegate: crate::keys::verification_key(&fixtures::key(2)),
                total: 100,
                funding_txid: TxID([2; 32]),
                vouchers: vec![
                    VoucherStatus {
                        id: [3; 32],
                        qty: 50,
                        state: VoucherState::Redeemed,
                        txid: Some(TxID([4; 32])),
                    },
                    VoucherStatus {
                        id: [5; 32],
                        qty: 50,
                        state: VoucherState::Unspent,
                        txid: None,
                    },
                ],
                recovery: vec![RecoveryStatus {
                    txid: TxID([6; 32]),
                    vouchers: vec![[5; 32]],
                    state: Progress::Pending,
                }],
            }],
        }
    }

    #[test]
    fn the_owner_snapshot_has_exactly_the_json_of_the_spec() {
        let status = owner_status();
        let expected = json!({
            "updated": 1_696_000_000,
            "tip_height": 12,
            "wallet": {
                "balance": "900",
                "outputs": [{"id": hex32(1), "qty": "900", "state": "unspent"}]
            },
            "allowances": [{
                "allowance": "0202020202020202",
                "merchant": status.allowances[0].merchant.to_bech32(crate::NETWORK),
                "delegate": hex::encode(status.allowances[0].delegate.to_bytes()),
                "total": "100",
                "funding_txid": hex32(2),
                "vouchers": [
                    {"id": hex32(3), "qty": "50", "state": "redeemed", "txid": hex32(4)},
                    {"id": hex32(5), "qty": "50", "state": "unspent", "txid": null}
                ],
                "recovery": [{"txid": hex32(6), "vouchers": [hex32(5)], "state": "pending"}]
            }]
        });
        assert_eq!(serde_json::to_value(&status).expect("serialize"), expected);
        let back: OwnerStatus = serde_json::from_value(expected).expect("deserialize");
        assert_eq!(back, status);
    }

    #[test]
    fn the_merchant_snapshot_has_exactly_the_json_of_the_spec() {
        let to = fixtures::address(3);
        let status = MerchantStatus {
            updated: 1_696_000_000,
            tip_height: 12,
            address: fixtures::address(1),
            receipts: vec![
                Receipt {
                    id: [1; 32],
                    qty: 50,
                    txid: TxID([2; 32]),
                    height: 7,
                    memo: "firebreak voucher".to_owned(),
                    spent: true,
                    spent_txid: Some(TxID([3; 32])),
                },
                Receipt {
                    id: [4; 32],
                    qty: 10,
                    txid: TxID([2; 32]),
                    height: 7,
                    memo: String::new(),
                    spent: false,
                    spent_txid: None,
                },
            ],
            balance: 10,
            spends: vec![Spend {
                txid: TxID([3; 32]),
                qty: 50,
                to,
                state: Progress::Confirmed,
            }],
        };
        let expected = json!({
            "updated": 1_696_000_000,
            "tip_height": 12,
            "address": fixtures::address(1).to_bech32(crate::NETWORK),
            "receipts": [
                {"id": hex32(1), "qty": "50", "txid": hex32(2), "height": 7,
                 "memo": "firebreak voucher", "spent": true, "spent_txid": hex32(3)},
                {"id": hex32(4), "qty": "10", "txid": hex32(2), "height": 7,
                 "memo": "", "spent": false, "spent_txid": null}
            ],
            "balance": "10",
            "spends": [{
                "txid": hex32(3), "qty": "50", "to": to.to_bech32(crate::NETWORK),
                "state": "confirmed"
            }]
        });
        assert_eq!(serde_json::to_value(&status).expect("serialize"), expected);
        let back: MerchantStatus = serde_json::from_value(expected).expect("deserialize");
        assert_eq!(back, status);
    }

    #[test]
    fn a_snapshot_with_a_bad_amount_or_state_is_refused() {
        let good = serde_json::to_value(owner_status()).expect("serialize");

        let mut number = good.clone();
        number["wallet"]["balance"] = json!(900);
        assert!(serde_json::from_value::<OwnerStatus>(number).is_err());

        let mut state = good.clone();
        state["allowances"][0]["vouchers"][0]["state"] = json!("spent");
        assert!(serde_json::from_value::<OwnerStatus>(state).is_err());

        let mut recovery = good;
        recovery["allowances"][0]["recovery"][0]["state"] = json!("done");
        assert!(serde_json::from_value::<OwnerStatus>(recovery).is_err());
    }

    #[test]
    fn a_snapshot_written_by_a_newer_build_still_reads() {
        // Readers of snapshots ignore fields they do not know, so the dashboard keeps working.
        let mut json = serde_json::to_value(owner_status()).expect("serialize");
        json["generated_by"] = json!("firebreak 0.2");
        json["wallet"]["note"] = json!("extra");
        assert!(serde_json::from_value::<OwnerStatus>(json).is_ok());
    }
}
