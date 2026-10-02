//! The status files the roles write for the dashboard, and the sources the dashboard reads.
//!
//! Every file is read into a typed structure and written out again from it, so only the fields
//! named here ever reach the page, whatever else a file holds. Times are unix seconds and amounts
//! are decimal strings of sparks, as in the files. Every field has a default, so a file that lacks
//! one still shows what it has.

use std::fs;
use std::io;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::node::{ContractChain, TxChain};

/// One source of the dashboard's data: a file, the agent or the node.
///
/// A source that is not `Ok` says why in `message`, in words for the person at the dashboard.
#[derive(Debug, Serialize)]
pub struct Source<T> {
    pub state: SourceState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
}

/// Whether a source of data could be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceState {
    /// The data is here.
    Ok,
    /// Nothing has written it yet.
    Missing,
    /// It exists but cannot be read.
    Error,
}

impl<T> Source<T> {
    pub fn ok(data: T) -> Source<T> {
        Source {
            state: SourceState::Ok,
            message: None,
            data: Some(data),
        }
    }

    pub fn missing<M>(message: M) -> Source<T>
    where
        M: Into<String>,
    {
        Source {
            state: SourceState::Missing,
            message: Some(message.into()),
            data: None,
        }
    }

    pub fn failed<M>(message: M) -> Source<T>
    where
        M: Into<String>,
    {
        Source {
            state: SourceState::Error,
            message: Some(message.into()),
            data: None,
        }
    }
}

impl<T> From<Result<T, String>> for Source<T> {
    fn from(result: Result<T, String>) -> Source<T> {
        match result {
            Ok(data) => Source::ok(data),
            Err(message) => Source::failed(message),
        }
    }
}

/// Reads the JSON status file at `path`. A file that does not exist yet is `Missing`.
pub fn read_status<T>(path: &Path) -> Source<T>
where
    T: DeserializeOwned,
{
    let name = path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Source::missing("not initialised yet");
        }
        Err(error) => return Source::failed(format!("cannot read {name}: {error}")),
    };
    match serde_json::from_str(&text) {
        Ok(status) => Source::ok(status),
        Err(error) => Source::failed(format!("{name} is not the expected JSON: {error}")),
    }
}

/// The owner's snapshot, `owner-status.json`.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct OwnerStatus {
    pub updated: Option<u64>,
    pub tip_height: Option<u64>,
    pub wallet: Wallet,
    pub allowances: Vec<OwnerAllowance>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Wallet {
    pub balance: String,
    pub outputs: Vec<WalletOutput>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct WalletOutput {
    pub id: String,
    pub qty: String,
    pub state: String,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct OwnerAllowance {
    pub allowance: String,
    pub merchant: String,
    pub delegate: String,
    pub total: String,
    pub funding_txid: String,
    pub vouchers: Vec<OwnerVoucher>,
    pub recovery: Vec<Recovery>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct OwnerVoucher {
    pub id: String,
    pub qty: String,
    pub state: String,
    pub txid: Option<String>,
    /// What the node says about the voucher, added by the dashboard and never read from a file.
    #[serde(skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub chain: Option<ContractChain>,
}

/// One recovery transaction of an allowance.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Recovery {
    pub txid: String,
    pub vouchers: Vec<String>,
    /// `pending` or `confirmed`, as the owner last recorded it.
    pub state: String,
    /// What the node says about the transaction, added by the dashboard.
    #[serde(skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub chain: Option<TxChain>,
}

/// The merchant's snapshot, `merchant-status.json`. This is the merchant's private view: it holds
/// the amounts and memos the merchant decrypted, and only the Merchant panel shows them.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct MerchantStatus {
    pub updated: Option<u64>,
    pub tip_height: Option<u64>,
    pub address: String,
    pub receipts: Vec<Receipt>,
    pub balance: String,
    pub spends: Vec<Spend>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Receipt {
    pub id: String,
    pub qty: String,
    pub txid: String,
    pub height: Option<u64>,
    pub memo: String,
    pub spent: bool,
    pub spent_txid: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Spend {
    pub txid: String,
    pub qty: String,
    pub to: String,
    pub state: String,
    /// What the node says about the transaction, added by the dashboard.
    #[serde(skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub chain: Option<TxChain>,
}

/// What the agent's `GET /api/status` answers. It holds the delegate's verification key, never
/// its secret.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentStatus {
    pub delegate: String,
    pub tip_height: Option<u64>,
    pub allowances: Vec<AgentAllowance>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentAllowance {
    pub allowance: String,
    pub merchant: String,
    pub total: String,
    pub vouchers: Vec<AgentVoucher>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentVoucher {
    pub id: String,
    pub qty: String,
    pub state: String,
    pub txid: Option<String>,
    /// What the node says about the voucher, added by the dashboard.
    #[serde(skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub chain: Option<ContractChain>,
}
