//! Why a merchant command failed.

use firebreak_core::ChainError;
use firebreak_core::store::StoreError;

/// Why a merchant command failed.
///
/// Every message starts with the stage that failed, so that a person can tell a request that was
/// refused from a failure of the prover, the signer or the node, and from an outcome that nobody
/// knows. The node's own words are always passed on unchanged.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The request is not one the merchant carries out. Nothing was done.
    #[error("refused: {0}")]
    Refused(String),

    /// The wallet's keys or outputs could not be worked out.
    #[error("wallet: {0}")]
    Wallet(firebreak_core::Error),

    /// The prover refused to build a transaction.
    #[error("prover: {0}")]
    Prover(firebreak_core::Error),

    /// Building a transaction again around fresh proofs gave a different transaction, which must
    /// never be submitted in place of the one that was saved.
    #[error(
        "prover: building the transaction again around fresh proofs gave a different one \
         (expected {expected}, got {found}); nothing more was submitted"
    )]
    Rebuilt {
        /// The transaction id that was saved before submitting.
        expected: String,
        /// The transaction id of the rebuilt transaction.
        found: String,
    },

    /// The signer refused to sign a transaction.
    #[error("signer: {0}")]
    Signer(firebreak_core::Error),

    /// The node refused a request or answered with an error, in its own words.
    #[error("node: {0}")]
    Node(ChainError),

    /// The node refused a transaction twice for a proof that did not hold, the second time with
    /// proofs that were fresh. Another transaction that is still waiting for a block almost
    /// certainly spends one of its inputs.
    #[error(
        "node: {0}; the proofs were fresh the second time, so another transaction that is still \
         pending probably spends one of its inputs, and the node holds neither attempt"
    )]
    Contested(ChainError),

    /// A transaction the node holds was not in a block by the deadline.
    #[error(
        "node: transaction {txid} was not confirmed within {seconds} s and is still pending; \
         run `firebreak-merchant inspect` later to see whether it confirmed"
    )]
    Unconfirmed {
        /// The transaction that is waiting.
        txid: String,
        /// How long the wait lasted.
        seconds: u64,
    },

    /// The node did not answer, so what became of the request is unknown.
    #[error("unknown: {cause}; {advice}")]
    Unknown {
        /// The transport failure.
        cause: ChainError,
        /// What the person should do about it.
        advice: String,
    },

    /// A file could not be read or written.
    #[error("files: {0}")]
    Store(#[from] StoreError),
}

impl Error {
    /// The error for a request to the node that failed while no transaction of ours was in
    /// flight, so that nothing is left in doubt.
    pub(crate) fn node(error: ChainError) -> Error {
        Error::chain(error, "no state was changed")
    }

    /// The error for a request to the node that failed, with `advice` for the person when the
    /// node gave no answer.
    pub(crate) fn chain(error: ChainError, advice: &str) -> Error {
        match error {
            ChainError::Transport(_) => Error::Unknown {
                cause: error,
                advice: advice.to_owned(),
            },
            other => Error::Node(other),
        }
    }

    /// The error for a wallet synchronization that failed.
    pub(crate) fn sync(error: firebreak_core::Error) -> Error {
        match error {
            firebreak_core::Error::Chain(error) => Error::node(error),
            other => Error::Wallet(other),
        }
    }
}
