//! Firebreak: limited payment authority on Flame through fixed-value, single-use vouchers.
//!
//! An owner funds an allowance as a set of vouchers. Each voucher locks a confidential token for
//! one merchant together with the receipt that merchant will need, under a predicate with exactly
//! two ways out: a delegated key can pay the whole token to that merchant, or the owner can take
//! it back. See [`voucher`] for the contract and [`build`] for the transactions around it.
//!
//! Around those sit the pieces every role's program shares: [`chain`] talks to the node,
//! [`wallet`] finds a wallet's outputs, [`store`] keeps each role's files, [`journal`] records
//! every submission attempt publicly, and [`select`] chooses the vouchers that pay an amount.

pub mod build;
pub mod chain;
#[cfg(feature = "devnet")]
pub mod devnet;
pub mod journal;
pub mod keys;
pub mod select;
pub mod store;
pub mod voucher;
pub mod wallet;

pub use build::{HEADER, LIMITS, Payout, WalletInput};
pub use chain::{Chain, ChainError};
pub use voucher::{Voucher, VoucherPolicy};

/// The network Firebreak runs on: every address is a testnet `tf1...` address.
pub const NETWORK: flamekd::Network = flamekd::Network::Testnet;

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

    #[error(
        "{0} payouts; a funding transaction makes 1 to {}",
        flamepayments::MAX_OUTPUTS
    )]
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

    #[error("address: {0}")]
    Address(#[from] flamekd::Error),

    #[error("key derivation: {0}")]
    Key(#[from] flamepayments::KeyError),

    #[error("node: {0}")]
    Chain(#[from] chain::ChainError),

    #[error("delegation package: {0}")]
    Package(#[from] store::PackageError),
}
