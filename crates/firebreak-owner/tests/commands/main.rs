//! The owner's commands against a node running in this process.
//!
//! Every test starts its own node and its own data directory. Tests that wait for a confirmation
//! run a block producer, as a real devnet has; tests that care exactly which block holds what
//! mint the blocks themselves.

mod cli;
mod files;
mod flow;
mod harness;
mod init;
mod persistence;
mod reclaim;
mod refusals;
