//! The node, as the roles use it: an asynchronous client of its JSON-RPC interface, with every
//! answer decoded into Firebreak's own types.
//!
//! Nothing here guesses. A question the node answers is decoded and checked against what was
//! asked: a contract must have the id it was fetched under, and a batch of answers must name
//! exactly the contracts asked about. A question the node does not answer, because the connection
//! failed or the reply was lost, is a [`ChainError::Transport`], which says that the outcome is
//! unknown. That matters most for [`Chain::submit`], where the node may have taken the
//! transaction before the reply was lost: the caller asks [`Chain::tx_state`] and
//! [`Chain::states`] before it builds a replacement.

use std::fmt;
use std::time::Duration;

use flamechain::codec::{contract_from_bytes, proof_from_bytes};
use flamechain::utreexo::Proof;
use flamed_rpc::{
    BlockTxEnvelope, ClientError, ContractId, FlamedApiClient, HttpClient, HttpClientBuilder,
    MAX_PROOF_IDS, MAX_SCAN_PREDICATES, PredicatePoint, ProofResult, ScanEntry, TxId,
    TxStatusResult, codes,
};
use flamevm::{Contract, TxID};
use tokio::time::{self, Instant};

/// A connection to a node.
///
/// Cloning is cheap, and clones share the connection.
#[derive(Clone)]
pub struct Chain {
    client: HttpClient,
    url: String,
}

impl Chain {
    /// A client for the node at `url`, such as `http://127.0.0.1:7740`.
    ///
    /// This only prepares the client. The node is first contacted by the first request, which
    /// fails with [`ChainError::Transport`] when nothing answers.
    pub fn connect(url: &str) -> Result<Chain, ChainError> {
        let client = HttpClientBuilder::default()
            .request_timeout(REQUEST_TIMEOUT)
            .build(url)
            .map_err(|error| ChainError::Url {
                url: url.to_owned(),
                reason: error.to_string(),
            })?;
        Ok(Chain {
            client,
            url: url.to_owned(),
        })
    }

    /// The URL the client talks to.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The height of the chain's tip. The genesis block is at height 0.
    pub async fn tip_height(&self) -> Result<u64, ChainError> {
        let tip = self.client.tip().await.map_err(call_error)?;
        Ok(tip.height)
    }

    /// What the node says about each contract in `ids`, in order.
    ///
    /// The node answers at most [`MAX_PROOF_IDS`] ids per request, so longer lists are asked in
    /// several. An answer that does not name exactly the contracts asked about, in order, is an
    /// error and not a guess.
    pub async fn states(&self, ids: &[[u8; 32]]) -> Result<Vec<ContractState>, ChainError> {
        let mut states = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(MAX_PROOF_IDS) {
            let asked = chunk.iter().copied().map(ContractId).collect();
            let answers = self.client.proofs(asked).await.map_err(call_error)?;
            if answers.len() != chunk.len() {
                return Err(ChainError::Decode(format!(
                    "asked about {} contracts and got {} answers",
                    chunk.len(),
                    answers.len()
                )));
            }
            for (id, (answered, result)) in chunk.iter().zip(answers) {
                if answered.0 != *id {
                    return Err(ChainError::Decode(format!(
                        "asked about contract {} and got an answer about {answered}",
                        hex::encode(id)
                    )));
                }
                states.push(ContractState::from_result(result)?);
            }
        }
        Ok(states)
    }

    /// A membership proof, valid against the tip as it is now, for each contract in `ids`, in
    /// order, which is the order a transaction's proofs must have.
    ///
    /// Every contract must be unspent: the first one that is not fails the call with
    /// [`ChainError::NotUnspent`]. A block that holds any transaction invalidates the proofs
    /// fetched before it, so fetch them right before building the transaction that carries them.
    pub async fn fresh_proofs(&self, ids: &[[u8; 32]]) -> Result<Vec<Proof>, ChainError> {
        let states = self.states(ids).await?;
        ids.iter()
            .zip(states)
            .map(|(id, state)| match state {
                ContractState::Unspent(proof) => Ok(proof),
                other => Err(ChainError::NotUnspent(*id, other.to_string())),
            })
            .collect()
    }

    /// The contract with id `id`, as the node archived it, or `None` when the node has none.
    pub async fn contract(&self, id: &[u8; 32]) -> Result<Option<Contract>, ChainError> {
        let result = match self.client.contract(ContractId(*id)).await {
            Ok(result) => result,
            Err(ClientError::Call(object)) if object.code() == codes::NOT_FOUND => {
                return Ok(None);
            }
            Err(error) => return Err(call_error(error)),
        };
        decode_contract(&result.bytes.0, id).map(Some)
    }

    /// Offers a packaged transaction, as [`crate::build::package`] makes it, to the node's
    /// mempool, and returns its transaction id.
    ///
    /// The node accepting a transaction does not confirm it: a block does, once someone mints
    /// one. A node that refuses the bytes gives [`ChainError::Rejected`] with its own words, which
    /// [`ChainError::is_stale_proof`] reads, and a lost answer gives [`ChainError::Transport`].
    /// Never send bytes again that were refused for a stale proof: build them again around fresh
    /// proofs.
    pub async fn submit(&self, tx: Vec<u8>) -> Result<TxID, ChainError> {
        let txid = self
            .client
            .submit_tx(BlockTxEnvelope(tx))
            .await
            .map_err(call_error)?;
        Ok(TxID(txid.0))
    }

    /// Where the transaction `txid` is: unknown to the node, waiting in its mempool, or in a
    /// block.
    pub async fn tx_state(&self, txid: &TxID) -> Result<TxState, ChainError> {
        let status = self
            .client
            .tx_status(TxId(txid.0))
            .await
            .map_err(call_error)?;
        Ok(match status {
            TxStatusResult::Unknown => TxState::Unknown,
            TxStatusResult::Mempool => TxState::Mempool,
            TxStatusResult::Confirmed { height, .. } => TxState::Confirmed { height },
        })
    }

    /// Waits until the transaction `txid` is in a block and returns the block's height, or fails
    /// with [`ChainError::Timeout`] once `timeout` has passed.
    ///
    /// The node is asked every 250 ms, and once more at the deadline, which needs a Tokio runtime
    /// with its timer enabled. A request that gets no answer ends the wait at once with
    /// [`ChainError::Transport`], since nothing more is known about the transaction.
    pub async fn wait_confirmed(&self, txid: &TxID, timeout: Duration) -> Result<u64, ChainError> {
        // A timeout too long to add to the clock never runs out.
        let deadline = Instant::now().checked_add(timeout);
        loop {
            match self.tx_state(txid).await? {
                TxState::Confirmed { height } => return Ok(height),
                TxState::Unknown | TxState::Mempool => {}
            }
            let pause = match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(ChainError::Timeout);
                    }
                    POLL_INTERVAL.min(deadline - now)
                }
                None => POLL_INTERVAL,
            };
            time::sleep(pause).await;
        }
    }

    /// Every contract created at `since` or later under one of the `predicates`, with the note
    /// that followed it in its transaction, oldest first.
    ///
    /// The node scans at most [`MAX_SCAN_PREDICATES`] predicates per request, so longer lists are
    /// asked in several. Each contract is checked against what the node said about it: its bytes
    /// must decode, hash to the id given and be locked by the predicate given.
    pub async fn scan(
        &self,
        predicates: &[[u8; 32]],
        since: u64,
    ) -> Result<Vec<ScanHit>, ChainError> {
        let mut unique = predicates.to_vec();
        unique.sort_unstable();
        unique.dedup();

        let mut hits = Vec::new();
        for chunk in unique.chunks(MAX_SCAN_PREDICATES) {
            let asked = chunk.iter().copied().map(PredicatePoint).collect();
            let answer = self.client.scan(asked, since).await.map_err(call_error)?;
            for entry in answer.outputs {
                hits.push(ScanHit::from_entry(entry)?);
            }
        }
        // Each request answers in height order; the stable sort merges them without reordering
        // contracts of one block.
        hits.sort_by_key(|hit| hit.height);
        Ok(hits)
    }
}

impl fmt::Debug for Chain {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Chain")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

/// What the node knows about one contract.
#[derive(Clone, Debug)]
pub enum ContractState {
    /// The contract is unspent, and the proof shows it against the tip the node answered at.
    Unspent(Proof),
    /// The contract was spent, by this transaction in a block of this height.
    Spent {
        /// The height of the block that spent it.
        height: u64,
        /// The transaction that spent it.
        txid: TxID,
    },
    /// The node has never seen the contract.
    Unknown,
}

impl ContractState {
    /// The state a proof query's answer stands for.
    fn from_result(result: ProofResult) -> Result<ContractState, ChainError> {
        Ok(match result {
            ProofResult::Unspent { proof } => {
                let proof = proof_from_bytes(&proof.0)
                    .map_err(|error| ChainError::Decode(format!("a membership proof: {error}")))?;
                ContractState::Unspent(proof)
            }
            ProofResult::Spent { height, txid } => ContractState::Spent {
                height,
                txid: TxID(txid.0),
            },
            ProofResult::Unknown => ContractState::Unknown,
        })
    }
}

impl fmt::Display for ContractState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContractState::Unspent(_) => formatter.write_str("unspent"),
            ContractState::Spent { height, txid } => write!(
                formatter,
                "spent at height {height} by transaction {}",
                hex::encode(txid.0)
            ),
            ContractState::Unknown => formatter.write_str("unknown to the node"),
        }
    }
}

/// Where a transaction is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxState {
    /// The node has neither a block nor a mempool entry for it.
    Unknown,
    /// It waits in the node's mempool for a block.
    Mempool,
    /// It is in the block at this height.
    Confirmed {
        /// The height of the block that holds it.
        height: u64,
    },
}

/// A contract a scan found.
#[derive(Clone, Debug)]
pub struct ScanHit {
    /// The contract's id.
    pub id: [u8; 32],
    /// The contract as published.
    pub contract: Contract,
    /// The data entry right after the contract's output in its transaction, which is the note a
    /// confidential output's recipient opens. `None` when no data followed.
    pub note: Option<Vec<u8>>,
    /// The height of the block that created the contract. Genesis allocations are at height 0.
    pub height: u64,
    /// The transaction that created the contract.
    pub txid: TxID,
    /// The height of the block that spent the contract and the transaction that did, or `None`
    /// while it is unspent.
    pub spent: Option<(u64, TxID)>,
}

impl ScanHit {
    /// The hit a scan entry describes, checked against its own contract.
    fn from_entry(entry: ScanEntry) -> Result<ScanHit, ChainError> {
        let contract = decode_contract(&entry.bytes.0, &entry.id.0)?;
        if contract.predicate.to_point().to_bytes() != entry.predicate.0 {
            return Err(ChainError::Decode(format!(
                "contract {} is not locked by the predicate the scan names",
                entry.id
            )));
        }
        Ok(ScanHit {
            id: entry.id.0,
            contract,
            note: entry.note.map(|note| note.0),
            height: entry.height,
            txid: TxID(entry.txid.0),
            spent: entry.spent.map(|spent| (spent.height, TxID(spent.txid.0))),
        })
    }
}

/// Why a request to the node failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ChainError {
    /// The node's address is not a usable URL.
    #[error("{url} is not a usable node address: {reason}")]
    Url {
        /// The address given.
        url: String,
        /// What is wrong with it.
        reason: String,
    },

    /// The node refused the bytes it was sent: it could not decode them, or its mempool did not
    /// admit them. The message is the node's own.
    #[error("the node refused the transaction (code {code}): {message}")]
    Rejected {
        /// The JSON-RPC error code.
        code: i32,
        /// The node's message, word for word.
        message: String,
    },

    /// The node answered with an error of another kind.
    #[error("the node answered with an error (code {code}): {message}")]
    Call {
        /// The JSON-RPC error code.
        code: i32,
        /// The node's message, word for word.
        message: String,
    },

    /// The connection failed or the answer was lost, so what became of the request is unknown.
    /// A transaction that was being submitted may or may not have reached the node.
    #[error("the node did not answer, so the outcome is unknown: {0}")]
    Transport(String),

    /// The node's answer is not what the protocol promises.
    #[error("the node's answer does not decode: {0}")]
    Decode(String),

    /// A contract that had to be unspent is not.
    #[error("contract {} is not unspent: {}", hex::encode(.0), .1)]
    NotUnspent([u8; 32], String),

    /// The transaction was not in a block by the deadline.
    #[error("the transaction was not confirmed in time")]
    Timeout,
}

impl ChainError {
    /// Whether the node refused a transaction because a membership proof in it did not hold.
    ///
    /// The mempool checks each proof against the tip as it is when the transaction arrives, so
    /// any block since the proof was fetched can make it fail. The cure is to fetch proofs again
    /// with [`Chain::fresh_proofs`], rebuild the transaction and submit it once more.
    ///
    /// A contract that was spent in the meantime is rejected in the same words, because its
    /// proof no longer proves anything. The refresh then ends in [`ChainError::NotUnspent`],
    /// which is how the caller finds out the transaction can never be admitted. A proof count
    /// that does not match the inputs is also counted here, as rebuilding the transaction with
    /// one fresh proof per input corrects it.
    pub fn is_stale_proof(&self) -> bool {
        let ChainError::Rejected { message, .. } = self else {
            return false;
        };
        PROOF_REJECTIONS
            .iter()
            .any(|rejection| message.contains(rejection))
    }
}

/// How long the client waits for the node to answer one request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How often [`Chain::wait_confirmed`] asks whether the transaction is in a block.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// The node's rejections that mean a membership proof did not hold, as the node words them.
///
/// The first two are the Utreexo accumulator's errors for an outdated proof and for a path that
/// does not lead to the contract. The third is the mempool's error for a transaction that does not
/// carry exactly one proof per input. The node reports a mempool error with the error's own text.
const PROOF_REJECTIONS: [&str; 3] = [
    "Item proof is outdated",
    "Merkle proof is invalid",
    "missing or trailing Utreexo proof",
];

/// The contract the bytes encode, if it has the id `id`.
fn decode_contract(bytes: &[u8], id: &[u8; 32]) -> Result<Contract, ChainError> {
    let contract = contract_from_bytes(bytes)
        .map_err(|error| ChainError::Decode(format!("contract {}: {error}", hex::encode(id))))?;
    if contract.id() != *id {
        return Err(ChainError::Decode(format!(
            "the bytes served as contract {} are another contract's",
            hex::encode(id)
        )));
    }
    Ok(contract)
}

/// The error a failed request stands for: the node's own error when it answered with one, and
/// otherwise a transport failure.
fn call_error(error: ClientError) -> ChainError {
    let ClientError::Call(object) = error else {
        return ChainError::Transport(error.to_string());
    };
    let code = object.code();
    let message = object.message().to_owned();
    match code {
        codes::MEMPOOL_REJECTED | codes::INVALID_BYTES => ChainError::Rejected { code, message },
        _ => ChainError::Call { code, message },
    }
}

#[cfg(test)]
mod tests {
    use flamed_rpc::ErrorObjectOwned;

    use super::*;

    fn rejected(message: &str) -> ChainError {
        ChainError::Rejected {
            code: codes::MEMPOOL_REJECTED,
            message: message.to_owned(),
        }
    }

    fn answered(code: i32, message: &str) -> ClientError {
        ClientError::Call(ErrorObjectOwned::owned(code, message, None::<()>))
    }

    #[test]
    fn a_refusal_over_a_membership_proof_is_a_stale_proof() {
        // The three texts the node reports, exactly as flamechain words them.
        for message in [
            "Item proof is outdated and must be re-created against the new state",
            "Merkle proof is invalid",
            "missing or trailing Utreexo proof",
        ] {
            assert!(rejected(message).is_stale_proof(), "{message}");
        }
    }

    #[test]
    fn other_refusals_and_other_errors_are_not_stale_proofs() {
        for message in [
            "duplicate transaction",
            "mempool policy limit reached",
            "transaction fee is below local policy",
            "invalid transaction envelope",
            "duplicate contract id",
            "the submitted bytes do not decode as a transaction under this chain's limits: x",
        ] {
            assert!(!rejected(message).is_stale_proof(), "{message}");
        }
        // The words alone do not make a stale proof: the error has to be the node's refusal.
        for error in [
            ChainError::Call {
                code: -32603,
                message: "Merkle proof is invalid".to_owned(),
            },
            ChainError::Transport("Merkle proof is invalid".to_owned()),
            ChainError::Decode("Merkle proof is invalid".to_owned()),
            ChainError::Timeout,
        ] {
            assert!(!error.is_stale_proof(), "{error}");
        }
    }

    #[test]
    fn the_nodes_refusals_keep_their_words_and_code() {
        for code in [codes::MEMPOOL_REJECTED, codes::INVALID_BYTES] {
            let error = call_error(answered(code, "duplicate transaction"));
            let ChainError::Rejected { code: got, message } = &error else {
                panic!("expected a refusal, got {error}");
            };
            assert_eq!((*got, message.as_str()), (code, "duplicate transaction"));
            assert!(error.to_string().contains("duplicate transaction"));
        }
    }

    #[test]
    fn other_answers_from_the_node_are_calls() {
        for code in [codes::NOT_FOUND, codes::LIMIT_EXCEEDED, -32603] {
            let error = call_error(answered(code, "some message"));
            let ChainError::Call { code: got, message } = &error else {
                panic!("expected a call error, got {error}");
            };
            assert_eq!((*got, message.as_str()), (code, "some message"));
        }
    }

    #[test]
    fn a_failure_to_get_an_answer_says_the_outcome_is_unknown() {
        for error in [
            ClientError::RequestTimeout,
            ClientError::Custom("connection reset".to_owned()),
            ClientError::InvalidSubscriptionId,
        ] {
            let error = call_error(error);
            assert!(matches!(error, ChainError::Transport(_)), "{error}");
            assert!(error.to_string().contains("outcome is unknown"), "{error}");
            assert!(!error.is_stale_proof());
        }
    }

    #[test]
    fn a_contract_state_describes_itself() {
        assert_eq!(ContractState::Unknown.to_string(), "unknown to the node");
        assert_eq!(
            ContractState::Unspent(Proof::Transient).to_string(),
            "unspent"
        );
        let spent = ContractState::Spent {
            height: 7,
            txid: TxID([0xab; 32]),
        };
        assert_eq!(
            spent.to_string(),
            format!("spent at height 7 by transaction {}", "ab".repeat(32))
        );
    }

    #[test]
    fn a_contract_that_is_not_unspent_names_itself_and_its_state() {
        let error = ChainError::NotUnspent([0x12; 32], "unknown to the node".to_owned());
        assert_eq!(
            error.to_string(),
            format!(
                "contract {} is not unspent: unknown to the node",
                "12".repeat(32)
            )
        );
    }

    #[test]
    fn a_proof_decodes_only_from_its_own_bytes() {
        let transient = flamechain::codec::proof_bytes(&Proof::Transient);
        let state = ContractState::from_result(ProofResult::Unspent {
            proof: transient.clone().into(),
        });
        assert!(matches!(
            state,
            Ok(ContractState::Unspent(Proof::Transient))
        ));

        let mut padded = transient;
        padded.push(0);
        let state = ContractState::from_result(ProofResult::Unspent {
            proof: padded.into(),
        });
        assert!(matches!(state, Err(ChainError::Decode(_))));
        let state = ContractState::from_result(ProofResult::Unspent {
            proof: Vec::new().into(),
        });
        assert!(matches!(state, Err(ChainError::Decode(_))));
    }

    #[test]
    fn spent_and_unknown_answers_keep_their_details() {
        let state = ContractState::from_result(ProofResult::Spent {
            height: 9,
            txid: TxId([5; 32]),
        });
        assert!(matches!(
            state,
            Ok(ContractState::Spent { height: 9, txid }) if txid == TxID([5; 32])
        ));
        assert!(matches!(
            ContractState::from_result(ProofResult::Unknown),
            Ok(ContractState::Unknown)
        ));
    }

    #[test]
    fn an_address_that_is_not_a_url_is_refused_before_any_request() {
        let error = Chain::connect("not a url").expect_err("not a url");
        assert!(
            matches!(&error, ChainError::Url { url, .. } if url == "not a url"),
            "{error}"
        );
        assert!(Chain::connect("http://127.0.0.1:7740").is_ok());
    }

    #[test]
    fn a_chain_shows_its_address_and_nothing_else() {
        let chain = Chain::connect("http://127.0.0.1:7740").expect("a client");
        assert_eq!(chain.url(), "http://127.0.0.1:7740");
        assert_eq!(
            format!("{chain:?}"),
            "Chain { url: \"http://127.0.0.1:7740\", .. }"
        );
    }

    #[tokio::test]
    async fn a_node_that_is_not_there_is_a_transport_failure_for_every_request() {
        // Nothing listens on port 1.
        let chain = Chain::connect("http://127.0.0.1:1").expect("a client");
        let txid = TxID([1; 32]);
        let id = [2u8; 32];
        assert!(matches!(
            chain.tip_height().await,
            Err(ChainError::Transport(_))
        ));
        assert!(matches!(
            chain.states(&[id]).await,
            Err(ChainError::Transport(_))
        ));
        assert!(matches!(
            chain.fresh_proofs(&[id]).await,
            Err(ChainError::Transport(_))
        ));
        assert!(matches!(
            chain.contract(&id).await,
            Err(ChainError::Transport(_))
        ));
        assert!(matches!(
            chain.submit(vec![1, 2, 3]).await,
            Err(ChainError::Transport(_))
        ));
        assert!(matches!(
            chain.tx_state(&txid).await,
            Err(ChainError::Transport(_))
        ));
        assert!(matches!(
            chain.scan(&[id], 0).await,
            Err(ChainError::Transport(_))
        ));
        assert!(matches!(
            chain.wait_confirmed(&txid, Duration::from_secs(1)).await,
            Err(ChainError::Transport(_))
        ));
        // Waiting without end must not overflow the clock.
        assert!(matches!(
            chain.wait_confirmed(&txid, Duration::MAX).await,
            Err(ChainError::Transport(_))
        ));
    }

    /// Whether `future` can run on a multi-threaded runtime.
    fn is_send<F>(_: &F)
    where
        F: Future + Send,
    {
    }

    #[test]
    fn every_request_can_run_on_a_multi_threaded_runtime() {
        let chain = Chain::connect("http://127.0.0.1:1").expect("a client");
        let txid = TxID([1; 32]);
        is_send(&chain.tip_height());
        is_send(&chain.states(&[]));
        is_send(&chain.fresh_proofs(&[]));
        is_send(&chain.contract(&[0; 32]));
        is_send(&chain.submit(Vec::new()));
        is_send(&chain.tx_state(&txid));
        is_send(&chain.wait_confirmed(&txid, Duration::ZERO));
        is_send(&chain.scan(&[], 0));
    }

    #[tokio::test]
    async fn asking_about_nothing_asks_nobody() {
        // No node is needed to learn that an empty list has an empty answer.
        let chain = Chain::connect("http://127.0.0.1:1").expect("a client");
        assert!(chain.states(&[]).await.expect("no states").is_empty());
        assert!(chain.fresh_proofs(&[]).await.expect("no proofs").is_empty());
        assert!(chain.scan(&[], 0).await.expect("no hits").is_empty());
    }
}
