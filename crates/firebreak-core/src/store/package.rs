//! The delegation package: what the owner gives the delegate when it funds an allowance.
//!
//! The package describes every voucher of an allowance: its id, its face value, the blinding of
//! its predicate tree and its contract as published. Together with the merchant, the delegate's
//! key and the owner's key, that is all a delegate needs to rebuild each voucher's
//! [`VoucherPolicy`] and reveal its redemption branch. None of it is secret from the delegate,
//! and the package holds nothing of the owner's: no key, no opening. In JSON:
//!
//! ```json
//! {
//!   "version": 1,
//!   "allowance": "<first 16 hex digits of the funding txid>",
//!   "merchant": "tf1...",
//!   "delegate": "<hex verification key>",
//!   "owner": "<hex verification key>",
//!   "funding_txid": "<hex>",
//!   "vouchers": [
//!     {"id": "<hex contract id>", "qty": "50", "blinding": "<hex>", "contract": "<hex bytes>"}
//!   ]
//! }
//! ```

use std::collections::BTreeSet;
use std::fmt;

use curve25519_dalek::ristretto::CompressedRistretto;
use flamechain::codec::{contract_bytes, contract_from_bytes};
use flamekd::ReceivingAddress;
use flamevm::TxID;
use serde::{Deserialize, Serialize};

use super::{VERSION, serde_address, serde_amount, serde_hex};
use crate::{Error, Voucher, VoucherPolicy};

/// An allowance as its owner hands it to the delegate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationPackage {
    /// The format version, [`VERSION`].
    pub version: u32,
    /// The allowance's id: the first 16 hex digits of its funding transaction's id.
    pub allowance: String,
    /// The merchant every voucher pays.
    #[serde(with = "serde_address")]
    pub merchant: ReceivingAddress,
    /// The key that redeems the vouchers.
    #[serde(with = "serde_hex")]
    pub delegate: CompressedRistretto,
    /// The key that recovers the vouchers.
    #[serde(with = "serde_hex")]
    pub owner: CompressedRistretto,
    /// The transaction that funded the vouchers.
    #[serde(with = "serde_hex")]
    pub funding_txid: TxID,
    /// The vouchers.
    pub vouchers: Vec<PackageVoucher>,
}

impl DelegationPackage {
    /// The package for `vouchers`, which a funding transaction `funding_txid` creates.
    ///
    /// Refused when there are no vouchers, or when they do not all name the same merchant,
    /// delegate and owner.
    pub fn from_vouchers(
        funding_txid: TxID,
        vouchers: &[Voucher],
    ) -> Result<DelegationPackage, Error> {
        let principals = Principals::of(vouchers.iter().map(|voucher| &voucher.policy))?;
        let entries = vouchers
            .iter()
            .map(|voucher| {
                Ok(PackageVoucher {
                    id: voucher.id(),
                    qty: voucher.qty,
                    blinding: voucher.policy.blinding,
                    contract: contract_bytes(&voucher.contract)?,
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        Ok(DelegationPackage {
            version: VERSION,
            allowance: allowance_id(&funding_txid),
            merchant: principals.merchant,
            delegate: principals.delegate,
            owner: principals.owner,
            funding_txid,
            vouchers: entries,
        })
    }

    /// The package's vouchers, each rebuilt under its policy.
    ///
    /// Nothing in the package is taken on trust. The version must be the one this build reads,
    /// the allowance id must belong to the funding transaction, and the ids must be distinct and
    /// the face values positive. Every contract must decode, be locked by the predicate its
    /// policy commits to, hold a voucher payload, and have the id the package lists for it.
    pub fn vouchers(&self) -> Result<Vec<Voucher>, Error> {
        self.check()?;
        self.vouchers
            .iter()
            .map(|entry| {
                let policy = VoucherPolicy {
                    merchant: self.merchant,
                    delegate: self.delegate,
                    owner: self.owner,
                    blinding: entry.blinding,
                };
                rebuild(policy, entry.id, &entry.contract, entry.qty)
            })
            .collect()
    }

    /// Checks everything about the package that does not need a contract decoded.
    fn check(&self) -> Result<(), PackageError> {
        if self.version != VERSION {
            return Err(PackageError::Version {
                found: self.version,
                expected: VERSION,
            });
        }
        if self.vouchers.is_empty() {
            return Err(PackageError::Empty);
        }
        if self.allowance != allowance_id(&self.funding_txid) {
            return Err(PackageError::AllowanceId(self.allowance.clone()));
        }
        let mut seen = BTreeSet::new();
        for entry in &self.vouchers {
            if entry.qty == 0 {
                return Err(PackageError::ZeroQuantity(entry.id));
            }
            if !seen.insert(entry.id) {
                return Err(PackageError::Duplicate(entry.id));
            }
        }
        Ok(())
    }
}

/// One voucher of a [`DelegationPackage`].
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageVoucher {
    /// The voucher's contract id.
    #[serde(with = "serde_hex")]
    pub id: [u8; 32],
    /// The voucher's face value in sparks, which only the owner can vouch for: the contract hides
    /// it.
    #[serde(with = "serde_amount")]
    pub qty: u64,
    /// The seed of the blinding leaves of the voucher's predicate tree.
    #[serde(with = "serde_hex")]
    pub blinding: [u8; 32],
    /// The voucher's contract as published, in the chain's encoding.
    #[serde(with = "serde_hex")]
    pub contract: Vec<u8>,
}

impl fmt::Debug for PackageVoucher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PackageVoucher")
            .field("id", &hex::encode(self.id))
            .field("qty", &self.qty)
            .finish_non_exhaustive()
    }
}

/// The id of the allowance that the funding transaction `funding_txid` creates: the first 16 hex
/// digits of the transaction id.
pub fn allowance_id(funding_txid: &TxID) -> String {
    hex::encode(&funding_txid.0[..8])
}

/// Why a delegation package, or an allowance made from one, is refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PackageError {
    /// The package is of a version this build does not read.
    #[error("package version {found}, but this build reads version {expected}")]
    Version {
        /// The version the package declares.
        found: u32,
        /// The version this build reads.
        expected: u32,
    },

    /// There are no vouchers.
    #[error("the package holds no vouchers")]
    Empty,

    /// The vouchers do not all name the same merchant, delegate and owner.
    #[error("the vouchers do not all name the same merchant, delegate and owner")]
    MixedParties,

    /// The allowance id is not the start of the funding transaction's id.
    #[error("allowance id {0} does not belong to the funding transaction")]
    AllowanceId(String),

    /// A voucher is listed twice.
    #[error("voucher {} is listed twice", hex::encode(.0))]
    Duplicate([u8; 32]),

    /// A voucher is worth nothing.
    #[error("voucher {} has a face value of zero", hex::encode(.0))]
    ZeroQuantity([u8; 32]),

    /// A voucher's listed id is not the id of its contract.
    #[error("voucher {} is listed under the id of another contract", hex::encode(.0))]
    IdMismatch([u8; 32]),

    /// The package delegates to another key than the importing agent's.
    #[error("the package delegates to a key that is not this agent's")]
    WrongDelegate,

    /// An allowance with the same id but other contents was imported before.
    #[error("allowance {0} was imported before with other contents")]
    Conflict(String),
}

/// The three keys and addresses that every voucher of an allowance shares.
pub(crate) struct Principals {
    pub(crate) merchant: ReceivingAddress,
    pub(crate) delegate: CompressedRistretto,
    pub(crate) owner: CompressedRistretto,
}

impl Principals {
    /// What `policies` have in common, if they all agree.
    pub(crate) fn of<'a, I>(policies: I) -> Result<Principals, PackageError>
    where
        I: IntoIterator<Item = &'a VoucherPolicy>,
    {
        let mut policies = policies.into_iter();
        let Some(first) = policies.next() else {
            return Err(PackageError::Empty);
        };
        let principals = Principals {
            merchant: first.merchant,
            delegate: first.delegate,
            owner: first.owner,
        };
        for policy in policies {
            let agrees = policy.merchant == principals.merchant
                && policy.delegate == principals.delegate
                && policy.owner == principals.owner;
            if !agrees {
                return Err(PackageError::MixedParties);
            }
        }
        Ok(principals)
    }
}

/// Rebuilds a voucher from what a store or package keeps of it, and checks the contract against
/// every claim: it must decode, sit under `policy`, hold a voucher payload and have id `id`.
pub(crate) fn rebuild(
    policy: VoucherPolicy,
    id: [u8; 32],
    contract: &[u8],
    qty: u64,
) -> Result<Voucher, Error> {
    let contract = contract_from_bytes(contract)?;
    let voucher = Voucher::new(policy, contract, qty)?;
    if voucher.id() != id {
        return Err(PackageError::IdMismatch(id).into());
    }
    Ok(voucher)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::store::fixtures::{self, Fixture};

    /// The JSON of a package, to tamper with.
    fn json_of(package: &DelegationPackage) -> serde_json::Value {
        serde_json::to_value(package).expect("serialize")
    }

    fn parse(json: serde_json::Value) -> Result<DelegationPackage, serde_json::Error> {
        serde_json::from_value(json)
    }

    #[test]
    fn a_package_has_exactly_the_json_of_the_spec() {
        let fixture = Fixture::new(&[50, 20]);
        let package = fixture.package();
        let json = json_of(&package);

        let funding = hex::encode(fixture.funding_txid.0);
        assert_eq!(json["version"], 1);
        assert_eq!(json["allowance"], funding[..16]);
        assert_eq!(json["merchant"], fixture.merchant.to_bech32(crate::NETWORK));
        assert_eq!(json["funding_txid"], funding);
        assert_eq!(
            json["delegate"],
            hex::encode(fixture.policy().delegate.to_bytes())
        );
        assert_eq!(
            json["owner"],
            hex::encode(fixture.policy().owner.to_bytes())
        );

        let mut keys: Vec<&str> = json
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "allowance",
                "delegate",
                "funding_txid",
                "merchant",
                "owner",
                "version",
                "vouchers"
            ]
        );

        let vouchers = json["vouchers"].as_array().expect("a list");
        assert_eq!(vouchers.len(), 2);
        for (entry, (voucher, _)) in vouchers.iter().zip(&fixture.vouchers) {
            assert_eq!(entry["id"], hex::encode(voucher.id()));
            assert_eq!(entry["qty"], voucher.qty.to_string());
            assert_eq!(entry["blinding"], hex::encode(voucher.policy.blinding));
            assert_eq!(
                entry["contract"],
                hex::encode(contract_bytes(&voucher.contract).expect("encode"))
            );
            assert_eq!(entry.as_object().expect("an object").len(), 4);
        }
    }

    #[test]
    fn a_package_rebuilds_the_vouchers_it_was_made_from() {
        let fixture = Fixture::new(&[50, 20, 20, 10]);
        let text = serde_json::to_string_pretty(&fixture.package()).expect("serialize");
        let package: DelegationPackage = serde_json::from_str(&text).expect("deserialize");
        let vouchers = package.vouchers().expect("valid vouchers");

        assert_eq!(vouchers.len(), 4);
        for (rebuilt, (original, _)) in vouchers.iter().zip(&fixture.vouchers) {
            assert_eq!(rebuilt.id(), original.id());
            assert_eq!(rebuilt.qty, original.qty);
            assert_eq!(rebuilt.policy, original.policy);
        }
    }

    #[test]
    fn a_package_carries_no_secret_of_the_owner() {
        let fixture = Fixture::new(&[50, 20]);
        let text = serde_json::to_string(&fixture.package()).expect("serialize");
        for (_, opening) in &fixture.vouchers {
            for secret in [
                opening.qty_blinding.to_bytes(),
                opening.flv_blinding.to_bytes(),
                fixture.owner_key.to_bytes(),
            ] {
                assert!(!text.contains(&hex::encode(secret)));
            }
        }
        assert!(!text.contains("opening"));
    }

    #[test]
    fn the_allowance_id_is_the_start_of_the_funding_txid() {
        assert_eq!(allowance_id(&TxID([0xab; 32])), "abababababababab");
        let mut txid = TxID([0; 32]);
        txid.0[..9].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(allowance_id(&txid), "0102030405060708");
    }

    #[test]
    fn vouchers_of_different_parties_do_not_make_one_package() {
        let fixture = Fixture::new(&[50, 20]);
        let mut vouchers = fixture.plain_vouchers();
        let other = fixtures::funded(
            VoucherPolicy {
                delegate: crate::keys::verification_key(&fixtures::key(99)),
                ..fixture.policy()
            },
            10,
            3,
        )
        .0;
        vouchers.push(other);
        let error = DelegationPackage::from_vouchers(fixture.funding_txid, &vouchers)
            .expect_err("mixed parties");
        assert!(
            matches!(error, Error::Package(PackageError::MixedParties)),
            "{error}"
        );

        let error =
            DelegationPackage::from_vouchers(fixture.funding_txid, &[]).expect_err("no vouchers");
        assert!(
            matches!(error, Error::Package(PackageError::Empty)),
            "{error}"
        );
    }

    #[test]
    fn a_package_of_another_version_is_refused() {
        let mut package = Fixture::new(&[50]).package();
        package.version = 2;
        let error = package.vouchers().expect_err("version 2");
        assert!(
            matches!(
                error,
                Error::Package(PackageError::Version {
                    found: 2,
                    expected: 1
                })
            ),
            "{error}"
        );
    }

    #[test]
    fn a_package_must_belong_to_its_funding_transaction() {
        let mut package = Fixture::new(&[50]).package();
        package.allowance = "0000000000000000".to_owned();
        let error = package.vouchers().expect_err("wrong allowance id");
        assert!(
            matches!(error, Error::Package(PackageError::AllowanceId(_))),
            "{error}"
        );
    }

    #[test]
    fn duplicate_zero_and_empty_vouchers_are_refused() {
        let package = Fixture::new(&[50, 20]).package();

        let mut duplicated = package.clone();
        duplicated.vouchers.push(duplicated.vouchers[0].clone());
        let error = duplicated.vouchers().expect_err("duplicate");
        assert!(
            matches!(error, Error::Package(PackageError::Duplicate(_))),
            "{error}"
        );

        let mut free = package.clone();
        free.vouchers[1].qty = 0;
        let error = free.vouchers().expect_err("zero quantity");
        assert!(
            matches!(error, Error::Package(PackageError::ZeroQuantity(_))),
            "{error}"
        );

        let mut empty = package;
        empty.vouchers.clear();
        let error = empty.vouchers().expect_err("no vouchers");
        assert!(
            matches!(error, Error::Package(PackageError::Empty)),
            "{error}"
        );
    }

    #[test]
    fn a_voucher_under_another_policy_is_refused() {
        // Another blinding commits to another predicate tree, which the contract is not locked by.
        let mut package = Fixture::new(&[50, 20]).package();
        package.vouchers[0].blinding = [0x42; 32];
        let error = package.vouchers().expect_err("wrong blinding");
        assert!(matches!(error, Error::PolicyMismatch), "{error}");

        // The same goes for another merchant, delegate or owner.
        let mut package = Fixture::new(&[50, 20]).package();
        package.delegate = crate::keys::verification_key(&fixtures::key(5));
        assert!(matches!(package.vouchers(), Err(Error::PolicyMismatch)));
        let mut package = Fixture::new(&[50, 20]).package();
        package.owner = crate::keys::verification_key(&fixtures::key(6));
        assert!(matches!(package.vouchers(), Err(Error::PolicyMismatch)));
        let mut package = Fixture::new(&[50, 20]).package();
        package.merchant = fixtures::address(77);
        assert!(matches!(package.vouchers(), Err(Error::PolicyMismatch)));
    }

    #[test]
    fn a_voucher_listed_under_another_id_is_refused() {
        let mut package = Fixture::new(&[50, 20]).package();
        package.vouchers[0].id = [0x01; 32];
        let error = package.vouchers().expect_err("wrong id");
        assert!(matches!(error, Error::Package(PackageError::IdMismatch(id)) if id == [1; 32]));
    }

    #[test]
    fn a_contract_that_does_not_decode_is_refused() {
        let mut package = Fixture::new(&[50]).package();
        package.vouchers[0].contract.truncate(40);
        assert!(matches!(package.vouchers(), Err(Error::Cell(_))));

        let mut package = Fixture::new(&[50]).package();
        package.vouchers[0].contract.push(0);
        assert!(matches!(package.vouchers(), Err(Error::Cell(_))));
    }

    #[test]
    fn a_package_that_is_not_the_expected_json_is_refused() {
        let good = json_of(&Fixture::new(&[50]).package());
        assert!(parse(good.clone()).is_ok());

        let mut extra = good.clone();
        extra["comment"] = json!("hello");
        assert!(parse(extra).is_err(), "unknown fields");

        let mut number = good.clone();
        number["vouchers"][0]["qty"] = json!(50);
        assert!(parse(number).is_err(), "an amount that is a number");

        let mut address = good.clone();
        address["merchant"] = json!("not an address");
        assert!(parse(address).is_err(), "a bad address");

        let mut key = good.clone();
        key["delegate"] = json!("ab".repeat(31));
        assert!(parse(key).is_err(), "a short key");

        let mut missing = good;
        missing.as_object_mut().expect("an object").remove("owner");
        assert!(parse(missing).is_err(), "a missing field");
    }

    #[test]
    fn debug_output_names_no_blinding_and_no_contract() {
        let fixture = Fixture::new(&[50]);
        let package = fixture.package();
        let shown = format!("{package:?}");
        assert!(shown.contains("PackageVoucher"));
        assert!(!shown.contains(&hex::encode(package.vouchers[0].blinding)));
        assert!(!shown.contains(&hex::encode(&package.vouchers[0].contract)));
    }
}
