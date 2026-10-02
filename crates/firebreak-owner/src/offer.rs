//! Offering a transaction to the node and journaling the attempt, and waiting for it to confirm.

use firebreak_core::ChainError;
use firebreak_core::journal::{self, Action, Actor, JournalEntry, Outcome, Stage};
use flamevm::TxID;

use crate::{Context, Error};

/// What the journal says about a transaction an attempt offers to the node.
pub(crate) struct Offer<'a> {
    /// What the transaction does.
    pub action: Action,
    /// The transaction's id, known before the node is asked.
    pub txid: TxID,
    /// The contracts the transaction spends.
    pub inputs: &'a [[u8; 32]],
    /// A short description for a reader of the journal. The journal is public, so it never says an
    /// amount.
    pub note: &'a str,
}

/// What came of an attempt.
pub(crate) struct Offered {
    /// The node's answer.
    pub result: Result<TxID, ChainError>,
    /// Why the attempt could not be journaled, when it could not.
    pub warning: Option<String>,
}

/// Offers `bytes` to the node and journals the attempt, whatever the node answers.
///
/// The journal line is written as soon as the node has answered, and names the answer word for
/// word. A journal that cannot be written is reported in [`Offered::warning`] and does not hide
/// the answer, which the caller has to act on.
pub(crate) async fn offer(ctx: &Context, offer: &Offer<'_>, bytes: Vec<u8>) -> Offered {
    let result = ctx.chain.submit(bytes.clone()).await;
    let (outcome, error) = match &result {
        Ok(_) => (Outcome::Accepted, None),
        Err(error @ ChainError::Transport(_)) => (Outcome::Unknown, Some(error.to_string())),
        Err(error) => (Outcome::Rejected, Some(error.to_string())),
    };
    let mut entry = JournalEntry::new(Actor::Owner, offer.action.clone(), Stage::Node, outcome);
    entry.txid = Some(offer.txid);
    entry.tx = Some(bytes);
    entry.inputs = offer.inputs.to_vec();
    entry.error = error;
    entry.note = Some(offer.note.to_owned());
    let warning = journal::append(&ctx.files.journal(), &entry)
        .err()
        .map(|error| format!("the journal could not be written: {error}"));
    Offered { result, warning }
}

/// Waits until the transaction `txid` is in a block and returns the block's height.
///
/// A transaction that is still waiting after [`Context::confirmation_timeout`] is not an
/// unknown outcome: the node holds it. A node that stops answering is.
pub(crate) async fn confirm(ctx: &Context, txid: &TxID) -> Result<u64, Error> {
    match ctx
        .chain
        .wait_confirmed(txid, ctx.confirmation_timeout)
        .await
    {
        Ok(height) => Ok(height),
        Err(ChainError::Timeout) => Err(Error::Unconfirmed {
            txid: hex::encode(txid.0),
            seconds: ctx.confirmation_timeout.as_secs(),
        }),
        Err(error) => Err(Error::chain(
            error,
            "the transaction may still confirm; run `firebreak-owner status` to find out",
        )),
    }
}
