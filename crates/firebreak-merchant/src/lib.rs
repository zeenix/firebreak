//! The merchant's side of Firebreak: finding the payments vouchers make, opening their receipts,
//! and spending what was received.
//!
//! The merchant holds only its own wallet. Everything else it learns from the node: a voucher's
//! redemption pays the merchant's published address, and the receipt that follows the payment
//! tells the merchant what it was paid. Each command is a function of a [`Context`], which is the
//! data directory's files plus a client of the node, and returns a report that displays as the
//! text the command prints. The `firebreak-merchant` program only parses its arguments and prints
//! what it gets.
//!
//! Two rules shape every command:
//!
//! * The private store is changed only under an exclusive lock, and the lock is never held while
//!   waiting for the node.
//! * What the node did not answer is unknown, not guessed: states stay as they were, and the
//!   error says to run `inspect`.

mod context;
mod error;
mod init;
mod inspect;
mod offer;
mod spend;

pub use context::{CONFIRMATION_TIMEOUT, Context, Files};
pub use error::Error;
pub use init::{InitReport, init};
pub use inspect::{InspectReport, inspect};
pub use spend::{Prepared, SpendReport, Spent, prepare, spend, submit};
