//! The dashboard's questions to the Flame node.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use flamed_rpc::{
    ClientError, ContractId, FlamedApiClient, HttpClient, HttpClientBuilder, MAX_PROOF_IDS,
    ProofResult, TxId, TxStatusResult,
};
use serde::Serialize;

use crate::explain::explain;

/// The chain's tip.
#[derive(Clone, Debug, Serialize)]
pub struct NodeTip {
    pub height: u64,
    pub hash: String,
}

/// What the node says about a transaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TxChain {
    /// Neither waiting in the mempool nor in a block.
    Unknown,
    /// Waiting in the mempool.
    Mempool,
    /// In a block.
    Confirmed { height: u64 },
    /// The node could not be asked, or refused to say.
    Unavailable { message: String },
}

/// What the node says about a contract, such as a voucher.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ContractChain {
    /// Unspent at the tip.
    Unspent,
    /// Spent by this transaction at this height.
    Spent { height: u64, txid: String },
    /// The node has never seen it.
    Unknown,
    /// The node could not be asked, or refused to say.
    Unavailable { message: String },
}

/// A JSON-RPC client for the node at `url`. Building it does not connect.
pub fn client(url: &str) -> Result<HttpClient, String> {
    HttpClientBuilder::default()
        .request_timeout(CALL_TIMEOUT)
        .build(url)
        .map_err(|error| format!("invalid node URL {url}: {}", explain(&error)))
}

/// One aggregation's questions to the node.
///
/// The first call that fails to reach the node fails every later call at once, so a node that is
/// down costs one timeout and not one per transaction. Transactions are asked about once.
pub struct Probe<'a> {
    client: &'a HttpClient,
    /// Why the node is unreachable, once a call has found it so.
    down: Option<String>,
    transactions: HashMap<String, TxChain>,
}

impl<'a> Probe<'a> {
    pub fn new(client: &'a HttpClient) -> Probe<'a> {
        Probe {
            client,
            down: None,
            transactions: HashMap::new(),
        }
    }

    /// The node's current tip.
    pub async fn tip(&mut self) -> Result<NodeTip, String> {
        if let Some(reason) = &self.down {
            return Err(reason.clone());
        }
        match self.client.tip().await {
            Ok(tip) => Ok(NodeTip {
                height: tip.height,
                hash: hex::encode(tip.hash.0),
            }),
            Err(error) => Err(self.fail(error)),
        }
    }

    /// Where the transaction `txid` is, according to the node.
    pub async fn transaction(&mut self, txid: &str) -> TxChain {
        let key = txid.to_ascii_lowercase();
        if let Some(known) = self.transactions.get(&key) {
            return known.clone();
        }
        let chain = self.ask_transaction(&key).await;
        self.transactions.insert(key, chain.clone());
        chain
    }

    /// Whether each of the contracts `ids` is unspent, spent or unknown, according to the node,
    /// keyed by lowercase ID. IDs that are not 32 bytes of hex are left out.
    pub async fn contracts(&mut self, ids: &[String]) -> HashMap<String, ContractChain> {
        let asked: Vec<(String, [u8; 32])> = ids
            .iter()
            .filter_map(|id| Some((id.to_ascii_lowercase(), parse_id(id)?)))
            .collect::<BTreeMap<_, _>>()
            .into_iter()
            .collect();
        let mut chains = HashMap::new();
        for batch in asked.chunks(MAX_PROOF_IDS) {
            chains.extend(self.ask_contracts(batch).await);
        }
        chains
    }

    async fn ask_transaction(&mut self, txid: &str) -> TxChain {
        let Some(id) = parse_id(txid) else {
            return TxChain::Unavailable {
                message: "not a transaction ID".to_owned(),
            };
        };
        if let Some(reason) = &self.down {
            return TxChain::Unavailable {
                message: reason.clone(),
            };
        }
        match self.client.tx_status(TxId(id)).await {
            Ok(TxStatusResult::Unknown) => TxChain::Unknown,
            Ok(TxStatusResult::Mempool) => TxChain::Mempool,
            Ok(TxStatusResult::Confirmed { height, .. }) => TxChain::Confirmed { height },
            Err(error) => TxChain::Unavailable {
                message: self.fail(error),
            },
        }
    }

    /// Asks about at most `MAX_PROOF_IDS` contracts in one call.
    async fn ask_contracts(
        &mut self,
        batch: &[(String, [u8; 32])],
    ) -> Vec<(String, ContractChain)> {
        if let Some(reason) = &self.down {
            return unavailable(batch, reason);
        }
        let ids = batch.iter().map(|(_, id)| ContractId(*id)).collect();
        match self.client.proofs(ids).await {
            Ok(answers) => answers
                .into_iter()
                .map(|(id, result)| (hex::encode(id.0), contract_chain(result)))
                .collect(),
            Err(error) => {
                let reason = self.fail(error);
                unavailable(batch, &reason)
            }
        }
    }

    /// Records a failed call and says why, in words.
    fn fail(&mut self, error: ClientError) -> String {
        match error {
            // The node answered with an error of its own, so it is up.
            ClientError::Call(object) => format!("the node refused: {}", object.message()),
            other => {
                let reason = format!("the node is unreachable: {}", explain(&other));
                self.down = Some(reason.clone());
                reason
            }
        }
    }
}

/// How long the node gets to answer one call. The dashboard polls every two seconds, so a node
/// that does not answer must not stall it.
const CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// Every contract of `batch`, as one the node could not be asked about.
fn unavailable(batch: &[(String, [u8; 32])], reason: &str) -> Vec<(String, ContractChain)> {
    batch
        .iter()
        .map(|(id, _)| {
            let message = reason.to_owned();
            (id.clone(), ContractChain::Unavailable { message })
        })
        .collect()
}

fn contract_chain(result: ProofResult) -> ContractChain {
    match result {
        ProofResult::Unspent { .. } => ContractChain::Unspent,
        ProofResult::Spent { height, txid } => ContractChain::Spent {
            height,
            txid: hex::encode(txid.0),
        },
        ProofResult::Unknown => ContractChain::Unknown,
    }
}

/// The 32 bytes a contract or transaction ID spells in hex.
fn parse_id(id: &str) -> Option<[u8; 32]> {
    hex::decode(id).ok()?.try_into().ok()
}
