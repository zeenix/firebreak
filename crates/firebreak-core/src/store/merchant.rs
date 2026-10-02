//! The merchant's private store: its wallet seed and what it has received and spent.
//!
//! The merchant holds only its own wallet. Everything about a payment it learns from the chain:
//! the receipts, whose amounts and memos it reads from the notes it opens, and the spends of what
//! it received.

use std::fmt;
use std::ops::Range;
use std::path::Path;

use flamekd::{ReceivingAddress, util};
use flamepayments::Account;
use flamevm::TxID;
use rand::{CryptoRng, RngCore};
use serde::{Deserialize, Serialize};

use super::{
    Progress, StoreError, VERSION, check_version, read, serde_address, serde_amount, serde_hex,
    write_private,
};
use crate::{Error, NETWORK, keys};

/// Everything the merchant keeps privately.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MerchantStore {
    /// The format version, [`VERSION`].
    pub version: u32,
    /// The seed of the merchant's wallet.
    #[serde(with = "serde_hex")]
    pub seed: [u8; 64],
    /// The next receiving address that has not been issued yet.
    pub next_index: u32,
    /// The payments the merchant has found.
    pub receipts: Vec<Receipt>,
    /// The transactions that spent what the merchant received.
    pub spends: Vec<Spend>,
}

impl MerchantStore {
    /// A store for a wallet, which has issued only its first address, the one the merchant
    /// publishes.
    pub fn new(seed: [u8; 64]) -> MerchantStore {
        MerchantStore {
            version: VERSION,
            seed,
            next_index: PUBLISHED_ADDRESSES,
            receipts: Vec::new(),
            spends: Vec::new(),
        }
    }

    /// A store with a random wallet seed.
    pub fn generate<R>(rng: &mut R) -> MerchantStore
    where
        R: RngCore + CryptoRng,
    {
        MerchantStore::new(keys::random_bytes(rng))
    }

    /// The store saved at `path`.
    pub fn load(path: &Path) -> Result<MerchantStore, StoreError> {
        let store: MerchantStore = read(path)?;
        check_version(path, store.version)?;
        Ok(store)
    }

    /// Saves the store to `path` as a private file.
    pub fn save(&self, path: &Path) -> Result<(), StoreError> {
        write_private(path, self)
    }

    /// The merchant's wallet.
    pub fn account(&self) -> Result<Account, Error> {
        Ok(Account::from_seed(&self.seed, NETWORK, self.next_index)?)
    }

    /// The address the merchant publishes, which the vouchers pay.
    pub fn address(&self) -> Result<ReceivingAddress, Error> {
        Ok(self.account()?.address_at(util::RECEIVING, 0)?)
    }

    /// The receiving addresses issued so far, as indices.
    pub fn receiving_range(&self) -> Range<u32> {
        0..self.next_index
    }

    /// Issues the next receiving address and counts it as used.
    ///
    /// The caller saves the store before it relies on the address, so that no address is ever
    /// issued twice.
    pub fn next_address(&mut self) -> Result<(u32, ReceivingAddress), Error> {
        let index = self.next_index;
        let address = self.account()?.address_at(util::RECEIVING, index)?;
        self.next_index = index.saturating_add(1);
        Ok((index, address))
    }
}

impl fmt::Debug for MerchantStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MerchantStore")
            .field("next_index", &self.next_index)
            .field("receipts", &self.receipts.len())
            .field("spends", &self.spends.len())
            .finish_non_exhaustive()
    }
}

/// A payment the merchant found and opened.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    /// The paid output's contract id.
    #[serde(with = "serde_hex")]
    pub id: [u8; 32],
    /// What the payment is worth, in sparks, as its note states.
    #[serde(with = "serde_amount")]
    pub qty: u64,
    /// The transaction that paid.
    #[serde(with = "serde_hex")]
    pub txid: TxID,
    /// The height of the block that holds it.
    pub height: u64,
    /// The note's memo, as text. Bytes that are not UTF-8 are replaced.
    pub memo: String,
    /// Whether the payment has been spent.
    pub spent: bool,
    /// The transaction that spent it, when it has been spent.
    #[serde(default, with = "serde_hex::option")]
    pub spent_txid: Option<TxID>,
}

impl fmt::Debug for Receipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Receipt")
            .field("id", &hex::encode(self.id))
            .field("height", &self.height)
            .field("spent", &self.spent)
            .finish_non_exhaustive()
    }
}

/// A transaction the merchant submitted to spend what it received.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spend {
    /// The spending transaction.
    #[serde(with = "serde_hex")]
    pub txid: TxID,
    /// What it moved, in sparks.
    #[serde(with = "serde_amount")]
    pub qty: u64,
    /// The address it paid.
    #[serde(with = "serde_address")]
    pub to: ReceivingAddress,
    /// Whether it confirmed.
    pub state: Progress,
}

/// The merchant's published address is its first receiving address, and spends go to the ones
/// after it.
const PUBLISHED_ADDRESSES: u32 = 1;

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use rand::SeedableRng;
    use rand::rngs::StdRng;
    use tempfile::TempDir;

    use super::*;
    use crate::store::fixtures;

    fn store() -> MerchantStore {
        let mut store = MerchantStore::generate(&mut StdRng::seed_from_u64(6));
        store.receipts.push(Receipt {
            id: [1; 32],
            qty: 50,
            txid: TxID([2; 32]),
            height: 7,
            memo: "firebreak voucher".to_owned(),
            spent: true,
            spent_txid: Some(TxID([3; 32])),
        });
        store.spends.push(Spend {
            txid: TxID([3; 32]),
            qty: 50,
            to: fixtures::address(9),
            state: Progress::Confirmed,
        });
        store
    }

    #[test]
    fn a_store_is_saved_privately_and_loaded_back_whole() {
        let dir = TempDir::new().expect("a temporary directory");
        let path = dir.path().join("merchant/merchant.json");
        let store = store();
        store.save(&path).expect("save");
        let mode = |path: &Path| fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().expect("a parent")), 0o700);

        let back = MerchantStore::load(&path).expect("load");
        assert_eq!(back.seed, store.seed);
        assert_eq!(back.next_index, store.next_index);
        assert_eq!(back.receipts, store.receipts);
        assert_eq!(back.spends, store.spends);
    }

    #[test]
    fn the_store_json_holds_a_hex_seed_and_decimal_string_amounts() {
        let json = serde_json::to_value(store()).expect("serialize");
        assert_eq!(json["seed"].as_str().expect("hex").len(), 128);
        assert_eq!(json["next_index"], 1);
        assert_eq!(json["receipts"][0]["qty"], "50");
        assert_eq!(json["receipts"][0]["spent"], true);
        assert_eq!(json["spends"][0]["state"], "confirmed");
        assert!(
            json["spends"][0]["to"]
                .as_str()
                .expect("text")
                .starts_with("tf1")
        );
    }

    #[test]
    fn the_published_address_is_the_first_and_spends_go_to_fresh_ones() {
        let mut store = MerchantStore::generate(&mut StdRng::seed_from_u64(1));
        let published = store.address().expect("an address");
        assert_eq!(store.receiving_range(), 0..1);

        let (first, first_address) = store.next_address().expect("an address");
        let (second, second_address) = store.next_address().expect("an address");
        assert_eq!((first, second), (1, 2));
        assert_eq!(store.receiving_range(), 0..3);
        assert_ne!(published, first_address);
        assert_ne!(first_address, second_address);
        // Issuing more addresses never moves the published one.
        assert_eq!(store.address().expect("an address"), published);
    }

    #[test]
    fn a_store_never_shows_a_secret_or_a_receipt_in_its_debug_output() {
        let store = store();
        let shown = format!("{store:?}{:?}", store.receipts);
        assert!(!shown.contains(&hex::encode(store.seed)));
        assert!(!shown.contains("firebreak voucher"));
        assert!(!shown.contains("qty") && !shown.contains("memo"), "{shown}");
        assert!(shown.contains("MerchantStore") && shown.contains("Receipt"));
    }
}
