//! Firebreak: limited payment authority on Flame through fixed-value, single-use vouchers.
//!
//! An owner funds an allowance as a set of vouchers. Each voucher locks a confidential token for
//! one merchant together with the receipt that merchant will need, under a predicate with exactly
//! two ways out: a delegated key can pay the whole token to that merchant, or the owner can take
//! it back. See [`voucher`] for the contract and [`build`] for the transactions around it.

pub mod build;
pub mod keys;
pub mod voucher;

pub use build::{HEADER, LIMITS, Payout, WalletInput};
pub use voucher::{Voucher, VoucherPolicy};

/// Why a Firebreak operation failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the contract is not locked under the voucher's policy")]
    PolicyMismatch,

    #[error("the contract does not hold a voucher payload")]
    NotAVoucher,

    #[error("the contract does not hold a wallet token")]
    NotAWalletOutput,

    #[error("the opening rebuilds a different contract")]
    OpeningMismatch,

    #[error("a transaction needs at least one input")]
    NoInputs,

    #[error("{0} payouts; a funding transaction makes 1 to {max}", max = flamepayments::MAX_OUTPUTS)]
    PayoutCount(usize),

    #[error("no key for the signature requirement of {}", hex::encode(.0.as_bytes()))]
    MissingKey(curve25519_dalek::ristretto::CompressedRistretto),

    #[error("VM: {0}")]
    Vm(#[from] flamevm::VMError),

    #[error("signing: {0}")]
    Musig(#[from] musig::MusigError),

    #[error("encoding: {0}")]
    Cell(#[from] flamevm::CellError),

    #[error("payments: {0}")]
    Builder(#[from] flamepayments::BuilderError),
}
