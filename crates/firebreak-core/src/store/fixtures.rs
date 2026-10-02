//! Vouchers and keys for the store tests, built without a node.
//!
//! A voucher here is exactly a funded voucher's contract: the policy's predicate locking a
//! payload of a confidential token and the merchant's receipt. Only the anchor, which a funding
//! transaction would assign, is made up.

use curve25519_dalek::scalar::Scalar;
use flamekd::{ReceivingAddress, util};
use flamepayments::{Account, Opening, OutputSpec, prepare_output};
use flamevm::{Anchor, Contract, FLAME_FLAVOR, Predicate, TxID};
use rand::SeedableRng;
use rand::rngs::StdRng;

use super::{DelegationPackage, OwnerAllowance};
use crate::{NETWORK, Voucher, VoucherPolicy, keys, voucher};

/// A deterministic signing key.
pub(crate) fn key(seed: u64) -> Scalar {
    keys::generate(&mut StdRng::seed_from_u64(seed))
}

/// A deterministic address of some wallet.
pub(crate) fn address(seed: u64) -> ReceivingAddress {
    let mut bytes = [0u8; 64];
    bytes[..8].copy_from_slice(&seed.to_le_bytes());
    Account::from_seed(&bytes, NETWORK, 0)
        .expect("an account")
        .address_at(util::RECEIVING, 0)
        .expect("an address")
}

/// A voucher of `qty` under `policy`, with the opening its owner keeps. `seed` makes the token's
/// blinding and the anchor deterministic.
pub(crate) fn funded(policy: VoucherPolicy, qty: u64, seed: u64) -> (Voucher, Opening) {
    let spec = OutputSpec {
        address: policy.merchant,
        qty,
        flv: FLAME_FLAVOR,
        memo: b"firebreak voucher".to_vec(),
    };
    let prepared =
        prepare_output(&spec, &mut StdRng::seed_from_u64(seed)).expect("prepare an output");
    let contract = Contract::new(
        Predicate::opaque(policy.predicate().expect("a predicate")),
        Anchor([seed as u8; 32]),
        voucher::payload(prepared.token, prepared.note),
    )
    .expect("a portable payload");
    let voucher = Voucher::new(policy, contract, qty).expect("a voucher");
    (voucher, prepared.opening)
}

/// An allowance of vouchers for one merchant, delegate and owner.
pub(crate) struct Fixture {
    pub(crate) merchant: ReceivingAddress,
    pub(crate) delegate_key: Scalar,
    pub(crate) owner_key: Scalar,
    pub(crate) funding_txid: TxID,
    pub(crate) vouchers: Vec<(Voucher, Opening)>,
}

impl Fixture {
    /// An allowance with one voucher per quantity.
    pub(crate) fn new(quantities: &[u64]) -> Fixture {
        let merchant = address(1);
        let delegate_key = key(2);
        let owner_key = key(3);
        let vouchers = quantities
            .iter()
            .enumerate()
            .map(|(index, qty)| {
                let policy = VoucherPolicy {
                    merchant,
                    delegate: keys::verification_key(&delegate_key),
                    owner: keys::verification_key(&owner_key),
                    blinding: [index as u8 + 1; 32],
                };
                funded(policy, *qty, 10 + index as u64)
            })
            .collect();
        Fixture {
            merchant,
            delegate_key,
            owner_key,
            funding_txid: TxID([0x5a; 32]),
            vouchers,
        }
    }

    /// The policy of the first voucher.
    pub(crate) fn policy(&self) -> VoucherPolicy {
        self.vouchers[0].0.policy
    }

    /// The vouchers without their openings.
    pub(crate) fn plain_vouchers(&self) -> Vec<Voucher> {
        self.vouchers
            .iter()
            .map(|(voucher, _)| voucher.clone())
            .collect()
    }

    /// The delegation package of the allowance.
    pub(crate) fn package(&self) -> DelegationPackage {
        DelegationPackage::from_vouchers(self.funding_txid, &self.plain_vouchers())
            .expect("a package")
    }

    /// The owner's record of the allowance, with every voucher `prepared`.
    pub(crate) fn owner_allowance(&self) -> OwnerAllowance {
        OwnerAllowance::prepared(self.funding_txid, &self.vouchers).expect("an allowance record")
    }
}
