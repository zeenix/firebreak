//! The payment agent against a real node.
//!
//! Every scenario starts a Flame node in this process, plays the owner by funding an allowance of
//! 50, 20, 20 and 10 sparks for the agent, and lets the agent import it. Most scenarios run with
//! a miner that mints a block every 200 ms, as a devnet does; those that need to decide when a
//! block comes mint it themselves.

mod api;
mod cli;
mod faults;
mod files;
mod fixture;
mod http;
mod pay;
mod proxy;
mod turn;
