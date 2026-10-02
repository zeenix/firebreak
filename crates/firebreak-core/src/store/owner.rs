//! The owner's private store: the wallet seed, the voucher authority key, and every allowance with
//! what recovering it takes.
//!
//! The store is a single file, `owner.json`, written privately. An allowance is saved with its
//! openings *before* its funding transaction is submitted, so that a crash after the submission
//! never leaves a funded voucher the owner cannot recover.

use std::fmt;
use std::ops::Range;
use std::path::Path;

use curve25519_dalek::ristretto::CompressedRistretto;
use curve25519_dalek::scalar::Scalar;
use flamechain::codec::contract_bytes;
use flamekd::{ReceivingAddress, util};
use flamepayments::{Account, Opening};
use flamevm::TxID;
use rand::{CryptoRng, RngCore};
use serde::{Deserialize, Serialize};

use super::package::{Principals, rebuild};
use super::{
    DelegationPackage, Progress, StoreError, VERSION, VoucherState, allowance_id, check_version,
    read, serde_address, serde_amount, serde_hex, serde_opening, write_private,
};
use crate::{Error, NETWORK, Voucher, VoucherPolicy, keys};

/// Everything the owner keeps privately.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerStore {
    /// The format version, [`VERSION`].
    pub version: u32,
    /// The seed of the owner's wallet.
    #[serde(with = "serde_hex")]
    pub wallet_seed: [u8; 64],
    /// The voucher authority: the key of every voucher's recovery branch.
    #[serde(with = "serde_hex")]
    pub authority_key: Scalar,
    /// The next change address of the wallet that has not been issued yet.
    pub next_change_index: u32,
    /// The wallet outputs as of the last reconciliation, and the outputs the owner expects.
    pub wallet: Vec<WalletOutput>,
    /// The allowances the owner funded.
    pub allowances: Vec<OwnerAllowance>,
}

impl OwnerStore {
    /// A store for a wallet and a voucher authority, with no allowances.
    pub fn new(wallet_seed: [u8; 64], authority_key: Scalar) -> OwnerStore {
        OwnerStore {
            version: VERSION,
            wallet_seed,
            authority_key,
            next_change_index: 0,
            wallet: Vec::new(),
            allowances: Vec::new(),
        }
    }

    /// A store with a random wallet seed and a random voucher authority.
    pub fn generate<R>(rng: &mut R) -> OwnerStore
    where
        R: RngCore + CryptoRng,
    {
        OwnerStore::new(keys::random_bytes(rng), keys::generate(rng))
    }

    /// The store saved at `path`.
    pub fn load(path: &Path) -> Result<OwnerStore, StoreError> {
        let store: OwnerStore = read(path)?;
        check_version(path, store.version)?;
        Ok(store)
    }

    /// Saves the store to `path` as a private file.
    pub fn save(&self, path: &Path) -> Result<(), StoreError> {
        write_private(path, self)
    }

    /// The owner's wallet.
    pub fn account(&self) -> Result<Account, Error> {
        Ok(Account::from_seed(
            &self.wallet_seed,
            NETWORK,
            RECEIVING_ISSUED,
        )?)
    }

    /// The verification key of the voucher authority.
    pub fn authority(&self) -> CompressedRistretto {
        keys::verification_key(&self.authority_key)
    }

    /// The change addresses issued so far, as indices.
    pub fn change_range(&self) -> Range<u32> {
        0..self.next_change_index
    }

    /// Issues the next change address and counts it as used.
    ///
    /// The caller saves the store before it relies on the address, so that no address is ever
    /// issued twice.
    pub fn next_change_address(&mut self) -> Result<(u32, ReceivingAddress), Error> {
        let index = self.next_change_index;
        let address = self.account()?.address_at(util::CHANGE, index)?;
        self.next_change_index = index.saturating_add(1);
        Ok((index, address))
    }

    /// The allowance with this id.
    pub fn allowance(&self, id: &str) -> Option<&OwnerAllowance> {
        self.allowances
            .iter()
            .find(|allowance| allowance.allowance == id)
    }

    /// The allowance with this id, to change.
    pub fn allowance_mut(&mut self, id: &str) -> Option<&mut OwnerAllowance> {
        self.allowances
            .iter_mut()
            .find(|allowance| allowance.allowance == id)
    }
}

impl fmt::Debug for OwnerStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OwnerStore")
            .field("next_change_index", &self.next_change_index)
            .field("wallet", &self.wallet.len())
            .field("allowances", &self.allowances.len())
            .finish_non_exhaustive()
    }
}

/// A wallet output the owner knows about.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WalletOutput {
    /// The output's contract id.
    #[serde(with = "serde_hex")]
    pub id: [u8; 32],
    /// The output's value in sparks.
    #[serde(with = "serde_amount")]
    pub qty: u64,
    /// The wallet branch of the address that holds it: receiving or change.
    pub branch: u32,
    /// The index of that address.
    pub index: u32,
    /// What spends the output when its token is confidential. A cleartext output, such as the
    /// genesis allocation, has none.
    #[serde(default, with = "serde_opening::option")]
    pub opening: Option<Opening>,
    /// Whether the output is spendable yet.
    pub state: VoucherState,
}

impl fmt::Debug for WalletOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WalletOutput")
            .field("id", &hex::encode(self.id))
            .field("branch", &self.branch)
            .field("index", &self.index)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

/// An allowance as its owner records it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerAllowance {
    /// The allowance's id: the first 16 hex digits of its funding transaction's id.
    pub allowance: String,
    /// The merchant every voucher pays.
    #[serde(with = "serde_address")]
    pub merchant: ReceivingAddress,
    /// The key that redeems the vouchers.
    #[serde(with = "serde_hex")]
    pub delegate: CompressedRistretto,
    /// The voucher authority's key, which recovers them.
    #[serde(with = "serde_hex")]
    pub owner: CompressedRistretto,
    /// The transaction that funds the vouchers.
    #[serde(with = "serde_hex")]
    pub funding_txid: TxID,
    /// The vouchers.
    pub vouchers: Vec<OwnerVoucher>,
    /// The recoveries the owner submitted.
    pub recoveries: Vec<Recovery>,
}

impl OwnerAllowance {
    /// The record of a freshly built allowance, before its funding transaction is submitted:
    /// every voucher is `prepared`, and the owner keeps `vouchers`' openings.
    ///
    /// Refused when there are no vouchers, or when they do not all name the same merchant,
    /// delegate and owner.
    pub fn prepared(
        funding_txid: TxID,
        vouchers: &[(Voucher, Opening)],
    ) -> Result<OwnerAllowance, Error> {
        let principals = Principals::of(vouchers.iter().map(|(voucher, _)| &voucher.policy))?;
        let records = vouchers
            .iter()
            .map(|(voucher, opening)| {
                Ok(OwnerVoucher {
                    id: voucher.id(),
                    qty: voucher.qty,
                    blinding: voucher.policy.blinding,
                    contract: contract_bytes(&voucher.contract)?,
                    opening: *opening,
                    state: VoucherState::Prepared,
                    txid: None,
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        Ok(OwnerAllowance {
            allowance: allowance_id(&funding_txid),
            merchant: principals.merchant,
            delegate: principals.delegate,
            owner: principals.owner,
            funding_txid,
            vouchers: records,
            recoveries: Vec::new(),
        })
    }

    /// What the vouchers are worth together, in sparks.
    pub fn total(&self) -> u64 {
        self.vouchers.iter().map(|voucher| voucher.qty).sum()
    }

    /// The vouchers, each rebuilt under its policy and checked against its contract.
    pub fn vouchers(&self) -> Result<Vec<Voucher>, Error> {
        self.vouchers
            .iter()
            .map(|record| {
                let policy = VoucherPolicy {
                    merchant: self.merchant,
                    delegate: self.delegate,
                    owner: self.owner,
                    blinding: record.blinding,
                };
                rebuild(policy, record.id, &record.contract, record.qty)
            })
            .collect()
    }

    /// The package that hands the allowance to its delegate.
    pub fn package(&self) -> Result<DelegationPackage, Error> {
        DelegationPackage::from_vouchers(self.funding_txid, &self.vouchers()?)
    }
}

/// A voucher as its owner records it: everything the owner needs to recover it.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerVoucher {
    /// The voucher's contract id.
    #[serde(with = "serde_hex")]
    pub id: [u8; 32],
    /// The voucher's face value in sparks.
    #[serde(with = "serde_amount")]
    pub qty: u64,
    /// The seed of the blinding leaves of the voucher's predicate tree.
    #[serde(with = "serde_hex")]
    pub blinding: [u8; 32],
    /// The voucher's contract in the chain's encoding.
    #[serde(with = "serde_hex")]
    pub contract: Vec<u8>,
    /// What rebuilds the voucher's token for the recovery transaction.
    #[serde(with = "serde_opening")]
    pub opening: Opening,
    /// Where the voucher stands, as last learned.
    pub state: VoucherState,
    /// The transaction that spent the voucher or is spending it, when one is known.
    #[serde(default, with = "serde_hex::option")]
    pub txid: Option<TxID>,
}

impl fmt::Debug for OwnerVoucher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OwnerVoucher")
            .field("id", &hex::encode(self.id))
            .field("qty", &self.qty)
            .field("state", &self.state)
            .field("txid", &self.txid.map(|txid| hex::encode(txid.0)))
            .finish_non_exhaustive()
    }
}

/// A recovery transaction the owner submitted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recovery {
    /// The recovery transaction.
    #[serde(with = "serde_hex")]
    pub txid: TxID,
    /// The vouchers it recovers.
    #[serde(with = "serde_hex::vec")]
    pub vouchers: Vec<[u8; 32]>,
    /// Whether it confirmed.
    pub state: Progress,
}

/// How many receiving addresses of the owner's wallet are in use: only the first, which holds the
/// genesis allocation.
const RECEIVING_ISSUED: u32 = 1;

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use rand::SeedableRng;
    use rand::rngs::StdRng;
    use tempfile::TempDir;

    use super::*;
    use crate::store::fixtures::{self, Fixture};

    fn store() -> OwnerStore {
        let mut store = OwnerStore::generate(&mut StdRng::seed_from_u64(8));
        let fixture = Fixture::new(&[50, 20, 20, 10]);
        store.allowances.push(fixture.owner_allowance());
        store.wallet.push(WalletOutput {
            id: [9; 32],
            qty: 900,
            branch: util::CHANGE,
            index: 0,
            opening: Some(fixture.vouchers[0].1),
            state: VoucherState::FundingPending,
        });
        store.wallet.push(WalletOutput {
            id: [8; 32],
            qty: 1000,
            branch: util::RECEIVING,
            index: 0,
            opening: None,
            state: VoucherState::Unspent,
        });
        store.next_change_index = 1;
        store
    }

    #[test]
    fn a_store_is_saved_privately_and_loaded_back_whole() {
        let dir = TempDir::new().expect("a temporary directory");
        let path = dir.path().join("owner/owner.json");
        let store = store();
        store.save(&path).expect("save");

        let mode = |path: &Path| fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().expect("a parent")), 0o700);

        let back = OwnerStore::load(&path).expect("load");
        assert_eq!(back.wallet_seed, store.wallet_seed);
        assert_eq!(back.authority_key, store.authority_key);
        assert_eq!(back.next_change_index, 1);
        assert_eq!(back.wallet.len(), 2);
        assert!(back.wallet[0].opening.is_some() && back.wallet[1].opening.is_none());
        assert_eq!(back.wallet[0].state, VoucherState::FundingPending);
        let (original, loaded) = (&store.allowances[0], &back.allowances[0]);
        assert_eq!(loaded.allowance, original.allowance);
        assert_eq!(loaded.funding_txid, original.funding_txid);
        assert_eq!(loaded.vouchers.len(), 4);
        for (loaded, original) in loaded.vouchers.iter().zip(&original.vouchers) {
            assert_eq!(loaded.id, original.id);
            assert_eq!(loaded.blinding, original.blinding);
            assert_eq!(loaded.contract, original.contract);
            assert_eq!(loaded.opening.qty, original.opening.qty);
            assert_eq!(loaded.opening.qty_blinding, original.opening.qty_blinding);
            assert_eq!(loaded.state, VoucherState::Prepared);
        }
    }

    #[test]
    fn a_store_of_another_version_or_with_unknown_fields_is_refused() {
        let dir = TempDir::new().expect("a temporary directory");
        let path = dir.path().join("owner.json");
        let mut json = serde_json::to_value(store()).expect("serialize");

        json["version"] = serde_json::json!(2);
        fs::write(&path, json.to_string()).expect("write");
        assert!(matches!(
            OwnerStore::load(&path),
            Err(StoreError::Version { found: 2, .. })
        ));

        json["version"] = serde_json::json!(1);
        json["note"] = serde_json::json!("hello");
        fs::write(&path, json.to_string()).expect("write");
        assert!(matches!(
            OwnerStore::load(&path),
            Err(StoreError::Json { .. })
        ));

        assert!(
            OwnerStore::load(&dir.path().join("absent.json"))
                .expect_err("absent")
                .is_not_found()
        );
    }

    #[test]
    fn the_store_json_holds_hex_keys_and_decimal_string_amounts() {
        let json = serde_json::to_value(store()).expect("serialize");
        assert_eq!(json["wallet_seed"].as_str().expect("hex").len(), 128);
        assert_eq!(json["authority_key"].as_str().expect("hex").len(), 64);
        assert_eq!(json["wallet"][0]["qty"], "900");
        assert!(json["wallet"][1]["opening"].is_null());
        let voucher = &json["allowances"][0]["vouchers"][0];
        assert_eq!(voucher["qty"], "50");
        assert_eq!(voucher["state"], "prepared");
        assert!(voucher["txid"].is_null());
        assert_eq!(voucher["opening"]["qty"], "50");
        assert!(
            json["allowances"][0]["merchant"]
                .as_str()
                .expect("text")
                .starts_with("tf1")
        );
    }

    #[test]
    fn a_store_never_shows_a_secret_in_its_debug_output() {
        let store = store();
        let mut shown = format!("{store:?}{:?}{:?}", store.wallet, store.allowances);
        shown.push_str(&format!("{:?}", store.allowances[0].vouchers[0]));
        let secrets = [
            hex::encode(store.wallet_seed),
            hex::encode(store.authority_key.to_bytes()),
            hex::encode(store.allowances[0].vouchers[0].blinding),
            hex::encode(
                store.allowances[0].vouchers[0]
                    .opening
                    .qty_blinding
                    .to_bytes(),
            ),
            hex::encode(
                store.allowances[0].vouchers[0]
                    .opening
                    .flv_blinding
                    .to_bytes(),
            ),
        ];
        for secret in secrets {
            assert!(!shown.contains(&secret), "a secret is in {shown}");
        }
        assert!(shown.contains("OwnerStore") && shown.contains("OwnerVoucher"));
    }

    #[test]
    fn change_addresses_are_issued_once_each() {
        let mut store = OwnerStore::generate(&mut StdRng::seed_from_u64(1));
        assert_eq!(store.change_range(), 0..0);
        let (first, first_address) = store.next_change_address().expect("an address");
        let (second, second_address) = store.next_change_address().expect("an address");
        assert_eq!((first, second), (0, 1));
        assert_ne!(first_address, second_address);
        assert_eq!(store.change_range(), 0..2);

        let account = store.account().expect("an account");
        assert_eq!(
            account.address_at(util::CHANGE, 1).expect("an address"),
            second_address
        );
    }

    #[test]
    fn the_authority_is_the_verification_key_of_the_authority_key() {
        let store = OwnerStore::generate(&mut StdRng::seed_from_u64(2));
        assert_eq!(
            store.authority(),
            keys::verification_key(&store.authority_key)
        );
    }

    #[test]
    fn a_prepared_allowance_records_every_voucher_with_its_opening() {
        let fixture = Fixture::new(&[50, 20, 20, 10]);
        let allowance = fixture.owner_allowance();

        assert_eq!(allowance.allowance, allowance_id(&fixture.funding_txid));
        assert_eq!(allowance.merchant, fixture.merchant);
        assert_eq!(allowance.total(), 100);
        assert!(allowance.recoveries.is_empty());
        for (record, (voucher, opening)) in allowance.vouchers.iter().zip(&fixture.vouchers) {
            assert_eq!(record.id, voucher.id());
            assert_eq!(record.qty, voucher.qty);
            assert_eq!(record.state, VoucherState::Prepared);
            assert_eq!(record.txid, None);
            assert_eq!(record.opening.qty, opening.qty);
            assert_eq!(record.opening.qty_blinding, opening.qty_blinding);
        }
    }

    #[test]
    fn an_allowance_rebuilds_its_vouchers_and_its_package() {
        let fixture = Fixture::new(&[50, 20]);
        let allowance = fixture.owner_allowance();
        let vouchers = allowance.vouchers().expect("vouchers");
        for (rebuilt, (original, _)) in vouchers.iter().zip(&fixture.vouchers) {
            assert_eq!(rebuilt.id(), original.id());
            assert_eq!(rebuilt.policy, original.policy);
        }
        assert_eq!(allowance.package().expect("a package"), fixture.package());
    }

    #[test]
    fn a_recovery_can_spend_a_voucher_rebuilt_from_the_record() {
        // The stored opening restores the token, so the record alone supports a recovery.
        let fixture = Fixture::new(&[50]);
        let allowance = fixture.owner_allowance();
        let voucher = &allowance.vouchers().expect("vouchers")[0];
        let record = &allowance.vouchers[0];
        let contract = voucher
            .with_opening(&record.opening)
            .expect("the opening matches");
        assert_eq!(contract.id(), record.id);
    }

    #[test]
    fn vouchers_that_disagree_on_their_parties_make_no_allowance() {
        let fixture = Fixture::new(&[50, 20]);
        let mut vouchers = fixture.vouchers.clone();
        let stranger = VoucherPolicy {
            merchant: fixtures::address(40),
            ..fixture.policy()
        };
        vouchers.push(fixtures::funded(stranger, 10, 5));
        let error = OwnerAllowance::prepared(fixture.funding_txid, &vouchers).expect_err("mixed");
        assert!(matches!(
            error,
            Error::Package(crate::store::PackageError::MixedParties)
        ));

        let error = OwnerAllowance::prepared(fixture.funding_txid, &[]).expect_err("none");
        assert!(matches!(
            error,
            Error::Package(crate::store::PackageError::Empty)
        ));
    }

    #[test]
    fn allowances_are_found_by_id() {
        let mut store = store();
        let id = store.allowances[0].allowance.clone();
        assert!(store.allowance(&id).is_some());
        assert!(store.allowance("ffffffffffffffff").is_none());
        store.allowance_mut(&id).expect("found").vouchers[0].state = VoucherState::Unspent;
        assert_eq!(store.allowances[0].vouchers[0].state, VoucherState::Unspent);
    }
}
