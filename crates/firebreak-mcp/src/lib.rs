//! A Model Context Protocol (MCP) server that lets an AI model use exactly one capability of the
//! Firebreak payment agent: paying the allowance's merchant.
//!
//! The server holds no keys and depends on no Flame crate. All it can do is ask the agent's local
//! HTTP API for two things, which [`Agent`] names: its status, and a payment. The agent's other
//! endpoints, such as the demo's reveal of the delegated key, and the owner's operations are out
//! of its reach. What may be spent, and where, is decided by the agent and the chain, not by this
//! server: it forwards each payment as the model asked for it and relays the agent's answer, so a
//! refusal by the agent's policy check reaches the model in the agent's own words.
//!
//! [`serve`] speaks MCP over any pair of async streams, one JSON-RPC message per line, as the
//! stdio transport does. The server offers the model two tools: `allowance_status`, which reads
//! the agent's status, and `pay`.

mod agent;
mod log;
mod server;
#[cfg(test)]
mod testing;
mod tools;

pub use agent::{Agent, Answer, HttpAgent, PayRequest, REQUEST_TIMEOUT};
pub use server::serve;
