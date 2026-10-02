//! The delegate's private store: its key and the allowances it imported.
//!
//! The delegate holds the key of every voucher's redemption branch and nothing else of value: no
//! owner key, no opening, no merchant key. The store keeps, per voucher, what the delegation
//! package said, plus the state and transaction of the voucher's redemption.

use std::fmt;
use std::path::Path;

use curve25519_dalek::ristretto::CompressedRistretto;
use curve25519_dalek::scalar::Scalar;
use flamekd::ReceivingAddress;
use flamevm::TxID;
use rand::{CryptoRng, RngCore};
use serde::{Deserialize, Serialize};

use super::package::rebuild;
use super::{
    DelegationPackage, PackageError, StoreError, VERSION, VoucherState, check_version, read,
    serde_address, serde_amount, serde_hex, write_private,
};
use crate::{Error, Voucher, VoucherPolicy, keys};

/// Everything the delegate keeps privately.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentStore {
    /// The format version, [`VERSION`].
    pub version: u32,
    /// The delegated key: the key of every voucher's redemption branch.
    #[serde(with = "serde_hex")]
    pub delegate_key: Scalar,
    /// The allowances imported from delegation packages.
    pub allowances: Vec<AgentAllowance>,
}

impl AgentStore {
    /// A store for a delegated key, with no allowances.
    pub fn new(delegate_key: Scalar) -> AgentStore {
        AgentStore {
            version: VERSION,
            delegate_key,
            allowances: Vec::new(),
        }
    }

    /// A store with a random delegated key.
    pub fn generate<R>(rng: &mut R) -> AgentStore
    where
        R: RngCore + CryptoRng,
    {
        AgentStore::new(keys::generate(rng))
    }

    /// The store saved at `path`.
    pub fn load(path: &Path) -> Result<AgentStore, StoreError> {
        let store: AgentStore = read(path)?;
        check_version(path, store.version)?;
        Ok(store)
    }

    /// Saves the store to `path` as a private file.
    pub fn save(&self, path: &Path) -> Result<(), StoreError> {
        write_private(path, self)
    }

    /// The verification key of the delegated key, which owners delegate to.
    pub fn public(&self) -> CompressedRistretto {
        keys::verification_key(&self.delegate_key)
    }

    /// Imports the allowance a delegation package describes, with every voucher in the state
    /// `unknown` until the agent has asked the node.
    ///
    /// The package is validated like [`DelegationPackage::vouchers`] does, and must delegate to
    /// this store's key. Importing the same allowance again changes nothing, and an allowance of
    /// the same id with other contents is refused.
    pub fn import(&mut self, package: &DelegationPackage) -> Result<Import, Error> {
        package.vouchers()?;
        if package.delegate != self.public() {
            return Err(PackageError::WrongDelegate.into());
        }
        let imported = AgentAllowance::from_package(package);
        let Some(existing) = self.allowance(&package.allowance) else {
            self.allowances.push(imported);
            return Ok(Import::Added);
        };
        if !existing.same_contents(&imported) {
            return Err(PackageError::Conflict(package.allowance.clone()).into());
        }
        Ok(Import::AlreadyImported)
    }

    /// The allowance with this id.
    pub fn allowance(&self, id: &str) -> Option<&AgentAllowance> {
        self.allowances
            .iter()
            .find(|allowance| allowance.allowance == id)
    }

    /// The allowance with this id, to change.
    pub fn allowance_mut(&mut self, id: &str) -> Option<&mut AgentAllowance> {
        self.allowances
            .iter_mut()
            .find(|allowance| allowance.allowance == id)
    }
}

impl fmt::Debug for AgentStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentStore")
            .field("allowances", &self.allowances.len())
            .finish_non_exhaustive()
    }
}

/// What [`AgentStore::import`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Import {
    /// The allowance is new to the store.
    Added,
    /// The store already held this allowance, which stays as it is.
    AlreadyImported,
}

/// An allowance as its delegate records it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentAllowance {
    /// The allowance's id: the first 16 hex digits of its funding transaction's id.
    pub allowance: String,
    /// The merchant every voucher pays, and the only one the agent pays.
    #[serde(with = "serde_address")]
    pub merchant: ReceivingAddress,
    /// The key that redeems the vouchers: this agent's.
    #[serde(with = "serde_hex")]
    pub delegate: CompressedRistretto,
    /// The key that recovers the vouchers: the owner's.
    #[serde(with = "serde_hex")]
    pub owner: CompressedRistretto,
    /// The transaction that funded the vouchers.
    #[serde(with = "serde_hex")]
    pub funding_txid: TxID,
    /// The vouchers.
    pub vouchers: Vec<AgentVoucher>,
}

impl AgentAllowance {
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

    /// The allowance a package describes, with every voucher `unknown`.
    fn from_package(package: &DelegationPackage) -> AgentAllowance {
        AgentAllowance {
            allowance: package.allowance.clone(),
            merchant: package.merchant,
            delegate: package.delegate,
            owner: package.owner,
            funding_txid: package.funding_txid,
            vouchers: package
                .vouchers
                .iter()
                .map(|entry| AgentVoucher {
                    id: entry.id,
                    qty: entry.qty,
                    blinding: entry.blinding,
                    contract: entry.contract.clone(),
                    state: VoucherState::Unknown,
                    txid: None,
                })
                .collect(),
        }
    }

    /// Whether `other` describes the same vouchers of the same funding, whatever their states.
    fn same_contents(&self, other: &AgentAllowance) -> bool {
        let described = |allowance: &AgentAllowance| {
            allowance
                .vouchers
                .iter()
                .map(|voucher| {
                    (
                        voucher.id,
                        voucher.qty,
                        voucher.blinding,
                        voucher.contract.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        self.merchant == other.merchant
            && self.delegate == other.delegate
            && self.owner == other.owner
            && self.funding_txid == other.funding_txid
            && described(self) == described(other)
    }
}

/// A voucher as its delegate records it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentVoucher {
    /// The voucher's contract id.
    #[serde(with = "serde_hex")]
    pub id: [u8; 32],
    /// The voucher's face value in sparks, as the owner stated it.
    #[serde(with = "serde_amount")]
    pub qty: u64,
    /// The seed of the blinding leaves of the voucher's predicate tree.
    #[serde(with = "serde_hex")]
    pub blinding: [u8; 32],
    /// The voucher's contract in the chain's encoding.
    #[serde(with = "serde_hex")]
    pub contract: Vec<u8>,
    /// Where the voucher stands, as last learned. A voucher reserved for a payment is
    /// `redemption_pending`, which keeps the agent from spending it twice.
    pub state: VoucherState,
    /// The redemption that spent the voucher or is spending it, when there is one.
    #[serde(default, with = "serde_hex::option")]
    pub txid: Option<TxID>,
}

impl fmt::Debug for AgentVoucher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentVoucher")
            .field("id", &hex::encode(self.id))
            .field("qty", &self.qty)
            .field("state", &self.state)
            .field("txid", &self.txid.map(|txid| hex::encode(txid.0)))
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use rand::SeedableRng;
    use rand::rngs::StdRng;
    use tempfile::TempDir;

    use super::*;
    use crate::store::fixtures::{self, Fixture};

    /// A store whose key is the fixture's delegated key.
    fn store(fixture: &Fixture) -> AgentStore {
        AgentStore::new(fixture.delegate_key)
    }

    #[test]
    fn importing_a_package_adds_its_vouchers_as_unknown() {
        let fixture = Fixture::new(&[50, 20, 20, 10]);
        let mut store = store(&fixture);
        let package = fixture.package();
        assert_eq!(store.import(&package).expect("import"), Import::Added);

        let allowance = store.allowance(&package.allowance).expect("imported");
        assert_eq!(allowance.merchant, fixture.merchant);
        assert_eq!(allowance.funding_txid, fixture.funding_txid);
        assert_eq!(allowance.total(), 100);
        assert_eq!(allowance.vouchers.len(), 4);
        for (record, (voucher, _)) in allowance.vouchers.iter().zip(&fixture.vouchers) {
            assert_eq!(record.id, voucher.id());
            assert_eq!(record.state, VoucherState::Unknown);
            assert_eq!(record.txid, None);
        }
        let vouchers = allowance.vouchers().expect("vouchers");
        assert_eq!(vouchers[0].policy, fixture.policy());
    }

    #[test]
    fn a_package_for_another_delegate_is_refused() {
        let fixture = Fixture::new(&[50]);
        let mut store = AgentStore::new(fixtures::key(77));
        let error = store
            .import(&fixture.package())
            .expect_err("another delegate");
        assert!(
            matches!(error, Error::Package(PackageError::WrongDelegate)),
            "{error}"
        );
        assert!(store.allowances.is_empty());
    }

    #[test]
    fn a_package_whose_vouchers_do_not_check_out_is_refused() {
        let fixture = Fixture::new(&[50, 20]);
        let mut store = store(&fixture);
        let mut package = fixture.package();
        package.vouchers[1].blinding = [0x77; 32];
        assert!(matches!(store.import(&package), Err(Error::PolicyMismatch)));
        assert!(store.allowances.is_empty());
    }

    #[test]
    fn importing_twice_keeps_what_the_agent_learned() {
        let fixture = Fixture::new(&[50, 20]);
        let mut store = store(&fixture);
        let package = fixture.package();
        store.import(&package).expect("import");

        let id = package.allowance.clone();
        let allowance = store.allowance_mut(&id).expect("imported");
        allowance.vouchers[0].state = VoucherState::RedemptionPending;
        allowance.vouchers[0].txid = Some(TxID([3; 32]));

        assert_eq!(
            store.import(&package).expect("again"),
            Import::AlreadyImported
        );
        let kept = &store.allowance(&id).expect("imported").vouchers[0];
        assert_eq!(kept.state, VoucherState::RedemptionPending);
        assert_eq!(kept.txid, Some(TxID([3; 32])));
        assert_eq!(store.allowances.len(), 1);
    }

    #[test]
    fn another_package_under_the_same_allowance_id_is_refused() {
        let fixture = Fixture::new(&[50, 20]);
        let mut store = store(&fixture);
        let package = fixture.package();
        store.import(&package).expect("import");

        // The same funding transaction cannot have created a different set of vouchers.
        let other = Fixture::new(&[50, 10]);
        let conflicting =
            DelegationPackage::from_vouchers(fixture.funding_txid, &other.plain_vouchers())
                .expect("a package");
        let error = store.import(&conflicting).expect_err("conflict");
        assert!(
            matches!(error, Error::Package(PackageError::Conflict(_))),
            "{error}"
        );
        assert_eq!(store.allowances.len(), 1);
    }

    #[test]
    fn a_store_is_saved_privately_and_loaded_back_whole() {
        let fixture = Fixture::new(&[50, 20]);
        let mut store = store(&fixture);
        store.import(&fixture.package()).expect("import");
        store.allowances[0].vouchers[1].state = VoucherState::Unspent;

        let dir = TempDir::new().expect("a temporary directory");
        let path = dir.path().join("agent/agent.json");
        store.save(&path).expect("save");
        let mode = |path: &Path| fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().expect("a parent")), 0o700);

        let back = AgentStore::load(&path).expect("load");
        assert_eq!(back.delegate_key, store.delegate_key);
        assert_eq!(back.allowances, store.allowances);
        assert_eq!(back.public(), keys::verification_key(&fixture.delegate_key));
    }

    #[test]
    fn the_store_json_has_no_place_for_an_owner_secret() {
        let fixture = Fixture::new(&[50]);
        let mut store = store(&fixture);
        store.import(&fixture.package()).expect("import");
        let text = serde_json::to_string(&store).expect("serialize");

        assert!(!text.contains("opening"));
        assert!(!text.contains(&hex::encode(fixture.owner_key.to_bytes())));
        for (_, opening) in &fixture.vouchers {
            assert!(!text.contains(&hex::encode(opening.qty_blinding.to_bytes())));
        }
        let json: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(json["allowances"][0]["vouchers"][0]["state"], "unknown");
        assert!(json["allowances"][0]["vouchers"][0]["txid"].is_null());
    }

    #[test]
    fn a_store_never_shows_a_secret_in_its_debug_output() {
        let fixture = Fixture::new(&[50]);
        let mut store = store(&fixture);
        store.import(&fixture.package()).expect("import");
        let shown = format!("{store:?}{:?}", store.allowances[0].vouchers[0]);
        assert!(!shown.contains(&hex::encode(fixture.delegate_key.to_bytes())));
        assert!(!shown.contains(&hex::encode(store.allowances[0].vouchers[0].blinding)));
        assert!(shown.contains("AgentStore") && shown.contains("AgentVoucher"));
    }

    #[test]
    fn a_generated_store_has_a_fresh_key_and_no_allowances() {
        let store = AgentStore::generate(&mut StdRng::seed_from_u64(4));
        assert_ne!(store.delegate_key, curve25519_dalek::scalar::Scalar::ZERO);
        assert!(store.allowances.is_empty());
        assert_eq!(store.version, VERSION);
    }
}
