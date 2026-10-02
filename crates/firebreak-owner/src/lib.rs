//! The owner's side of Firebreak: funding allowances and taking back what the delegate leaves.
//!
//! The owner holds the wallet, the voucher authority key and the opening of every voucher. It is
//! the only role that funds an allowance or recovers one. Each command is a function of a
//! [`Context`], which is the data directory's files plus a client of the node, and returns a
//! report that displays as the text the command prints. The `firebreak-owner` program only parses
//! its arguments and prints what it gets.
//!
//! Three rules shape every command:
//!
//! * What recovers a voucher is saved before anything that could fund it is sent, so a crash or a
//!   lost answer never leaves value the owner cannot take back.
//! * The private store is changed only under an exclusive lock, and the lock is never held while
//!   waiting for the node.
//! * What the node did not answer is unknown, not guessed: states stay as they were, and the
//!   error says to run `status`.

mod allowance;
mod context;
mod error;
mod init;
mod offer;
mod reclaim;
mod status;

pub use allowance::{
    AllowanceReport, CreateAllowance, MAX_PAYOUTS, Prepared, VoucherLine, create_allowance,
    parse_delegate, parse_merchant, parse_vouchers, prepare, submit,
};
pub use context::{CONFIRMATION_TIMEOUT, Context, Files};
pub use error::Error;
pub use init::{DEFAULT_GENESIS_SPARKS, InitReport, init};
pub use reclaim::{Reclaim, ReclaimReport, Recovered, Skip, reclaim};
pub use status::{StatusReport, status};
