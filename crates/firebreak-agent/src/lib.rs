//! The Firebreak payment agent: the app that holds a delegated key and pays one merchant out of
//! an allowance.
//!
//! The agent keeps only what a delegate needs: the delegated key and the public descriptors of
//! the vouchers an owner funded for it, which arrive as a [delegation package]. It has no owner
//! key, no opening and no merchant key, so the most it can do with an allowance is what the
//! vouchers allow: redeem them, whole, to their one merchant. The checks the agent makes before
//! it pays are a courtesy. The chain enforces the destination and the amount whatever this
//! program does.
//!
//! [`Agent`] is a process's handle on its [`Files`] and its node. Its operations are
//! [`Agent::pay`] and [`Agent::status`], and, without the node, [`Files::init`] and
//! [`import::packages`]. [`api`] serves payment and status over HTTP, and the binary is a thin
//! shell around all of them.
//!
//! # One agent, many processes
//!
//! The command line and the server run at the same time on one data directory: the dashboard
//! polls the server's status while a person pays from the command line. Two locks keep them
//! from interfering. Every change of `agent.json` is a read-modify-write under an exclusive lock
//! on `agent/lock`, which is held for file operations only. And the [`Turn`] orders everything
//! that acts on what the node says, so that a reconciliation never meets a payment halfway
//! between reserving its vouchers and submitting them.
//!
//! # The delegated key's reveal
//!
//! `serve --reveal-key` adds an endpoint that gives the delegated secret key to anyone who asks.
//! It is the demonstration's deliberate key leak, and nothing else in the agent prints, logs or
//! serves the key.
//!
//! [delegation package]: firebreak_core::store::DelegationPackage

pub mod api;
pub mod import;
pub mod pay;
pub mod status;

mod agent;
mod error;
mod files;
mod reconcile;

pub use agent::{Agent, Turn};
pub use error::Error;
pub use files::Files;
pub use pay::{PayError, PayRequest, Payment};
pub use status::Status;
