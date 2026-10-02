//! Running attacks against a live node: choosing a target, crafting the candidate, verifying it
//! the way the chain will, submitting it directly to the node, and recording exactly where it was
//! stopped.
//!
//! Every attempt is checked against the node before and after: a refused attack must leave its
//! target voucher exactly as it found it.

use std::path::PathBuf;

use firebreak_core::build;
use firebreak_core::chain::{Chain, ChainError, ContractState};
use firebreak_core::journal::{self, Action, Actor, JournalEntry, Outcome, Stage};
use firebreak_core::store::StoreError;
use firebreak_core::{LIMITS, Voucher};
use flamechain::utreexo::Proof;
use flamechain::{BlockTx, ChainParams};
use flamevm::TxID;

use crate::{Adversary, Attack, Candidate, Stopped, craft};

/// What an attack runs with.
pub struct Context {
    /// The node, reached directly.
    pub chain: Chain,
    /// The adversary's keys.
    pub adversary: Adversary,
    /// Every voucher the adversary learned of from public descriptors.
    pub vouchers: Vec<Voucher>,
    /// The public journal every attempt is appended to, when there is one.
    pub journal: Option<PathBuf>,
}

/// Where a candidate was stopped, if anywhere, in the words of whatever stopped it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The prover refused to build the transaction, so no bytes ever existed.
    Prover(String),
    /// The signer refused to sign it.
    Signer(String),
    /// The node refused the bytes. `verifier` is what a standalone verification of the same
    /// bytes said first, or `None` when it accepted them.
    Node {
        verifier: Option<String>,
        error: String,
    },
    /// The submission's outcome is unknown: the node did not answer.
    Unknown {
        verifier: Option<String>,
        error: String,
    },
    /// The node admitted the transaction.
    Accepted {
        txid: TxID,
        verifier: Option<String>,
    },
}

impl Verdict {
    /// The error that stopped the candidate, in the words of the stage that raised it.
    pub fn error(&self) -> Option<&str> {
        match self {
            Verdict::Prover(error)
            | Verdict::Signer(error)
            | Verdict::Node { error, .. }
            | Verdict::Unknown { error, .. } => Some(error),
            Verdict::Accepted { .. } => None,
        }
    }

    /// What a standalone verification of the candidate's bytes said, when it refused them.
    pub fn verifier(&self) -> Option<&str> {
        match self {
            Verdict::Node { verifier, .. }
            | Verdict::Unknown { verifier, .. }
            | Verdict::Accepted { verifier, .. } => verifier.as_deref(),
            Verdict::Prover(_) | Verdict::Signer(_) => None,
        }
    }
}

/// One attempt, from target to verdict.
pub struct Report {
    /// The attack.
    pub attack: Attack,
    /// The voucher it targeted.
    pub target: [u8; 32],
    /// The target's face value.
    pub target_qty: u64,
    /// The target's state on the node before the attempt.
    pub before: String,
    /// What happened to the candidate.
    pub verdict: Verdict,
    /// The target's state on the node after the attempt.
    pub after: String,
}

impl Report {
    /// Whether the outcome is the one the threat model predicts: refused for every attack except
    /// [`Attack::ReceiptSwap`], which is a valid payment the node must admit.
    pub fn as_expected(&self) -> bool {
        let admitted = matches!(self.verdict, Verdict::Accepted { .. });
        admitted == self.attack.is_valid_payment()
            && !matches!(self.verdict, Verdict::Unknown { .. })
    }

    /// Whether the node admitted an attack that is not a valid payment: a security failure.
    pub fn is_breach(&self) -> bool {
        matches!(self.verdict, Verdict::Accepted { .. }) && !self.attack.is_valid_payment()
    }
}

/// Why an attack could not be run at all.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("no voucher {} is known from the public descriptors", hex::encode(.0))]
    UnknownVoucher([u8; 32]),

    #[error("no voucher is {0} on the node; pass --voucher to choose one")]
    NoTarget(&'static str),

    #[error(transparent)]
    Chain(#[from] ChainError),

    #[error(transparent)]
    Store(#[from] StoreError),

    #[error(transparent)]
    Stopped(#[from] Stopped),

    #[error("packaging the candidate: {0}")]
    Package(#[from] firebreak_core::Error),
}

/// Runs `attack` against `target`, or against a voucher in the state the attack needs: a spent
/// one for [`Attack::RedeemSpent`], an unspent one for every other attack.
pub async fn attempt(
    context: &Context,
    attack: Attack,
    target: Option<[u8; 32]>,
) -> Result<Report, RunError> {
    let voucher = choose(context, attack, target).await?;
    let before = state_of(&context.chain, &voucher.id()).await?;

    let action = Action::Attack(attack.name().to_owned());
    let (verdict, mut entry) = match craft(attack, &[voucher], &context.adversary) {
        Ok(candidate) => submit(context, action, candidate).await?,
        Err(stopped) => {
            let (stage, verdict) = match stopped {
                Stopped::Prover(error) => (Stage::Prover, Verdict::Prover(error.to_string())),
                Stopped::Signer(error) => (Stage::Signer, Verdict::Signer(error.to_string())),
                other => return Err(other.into()),
            };
            let mut entry = JournalEntry::new(Actor::Attacker, action, stage, Outcome::Rejected);
            entry.inputs = vec![voucher.id()];
            entry.error = verdict.error().map(str::to_owned);
            (verdict, entry)
        }
    };
    if let Some(path) = &context.journal {
        entry.note = Some(match verdict.verifier() {
            Some(verifier) => format!(
                "{} A standalone verification of the same bytes said: {verifier}",
                attack.description()
            ),
            None => attack.description().to_owned(),
        });
        journal::append(path, &entry)?;
    }

    let after = state_of(&context.chain, &voucher.id()).await?;
    Ok(Report {
        attack,
        target: voucher.id(),
        target_qty: voucher.qty,
        before,
        verdict,
        after,
    })
}

/// The voucher to attack.
async fn choose(
    context: &Context,
    attack: Attack,
    target: Option<[u8; 32]>,
) -> Result<&Voucher, RunError> {
    if let Some(id) = target {
        return context
            .vouchers
            .iter()
            .find(|voucher| voucher.id() == id)
            .ok_or(RunError::UnknownVoucher(id));
    }
    let ids: Vec<[u8; 32]> = context.vouchers.iter().map(Voucher::id).collect();
    let states = context.chain.states(&ids).await?;
    let wanted = |state: &ContractState| match attack {
        Attack::RedeemSpent => matches!(state, ContractState::Spent { .. }),
        _ => matches!(state, ContractState::Unspent(_)),
    };
    context
        .vouchers
        .iter()
        .zip(&states)
        .find(|(_, state)| wanted(state))
        .map(|(voucher, _)| voucher)
        .ok_or(RunError::NoTarget(match attack {
            Attack::RedeemSpent => "spent",
            _ => "unspent",
        }))
}

/// Packages `candidate` with the best proofs the adversary can get, verifies the bytes the way
/// the chain does, and submits them directly to the node.
async fn submit(
    context: &Context,
    action: Action,
    candidate: Candidate,
) -> Result<(Verdict, JournalEntry), RunError> {
    let Candidate { tx, inputs } = candidate;
    let claimed = tx.txid;
    // A spent contract has no current proof. The adversary claims it anyway, with the transient
    // proof a contract created in the same block would carry, and lets the node decide.
    let proofs = context
        .chain
        .states(&inputs)
        .await?
        .into_iter()
        .map(|state| match state {
            ContractState::Unspent(proof) => proof,
            ContractState::Spent { .. } | ContractState::Unknown => Proof::Transient,
        })
        .collect();
    let bytes = build::package(tx, proofs)?;
    let verifier = verify(&bytes);

    let mut entry = JournalEntry::new(Actor::Attacker, action, Stage::Node, Outcome::Rejected);
    entry.txid = Some(claimed);
    entry.tx = Some(bytes.clone());
    entry.inputs = inputs;
    let verdict = match context.chain.submit(bytes).await {
        Ok(txid) => {
            entry.outcome = Outcome::Accepted;
            entry.txid = Some(txid);
            Verdict::Accepted { txid, verifier }
        }
        Err(error @ ChainError::Transport(_)) => {
            entry.outcome = Outcome::Unknown;
            entry.error = Some(error.to_string());
            Verdict::Unknown {
                verifier,
                error: error.to_string(),
            }
        }
        Err(error) => {
            entry.error = Some(error.to_string());
            Verdict::Node {
                verifier,
                error: error.to_string(),
            }
        }
    };
    Ok((verdict, entry))
}

/// What a standalone verification of the packaged bytes says: `None` when they verify.
fn verify(bytes: &[u8]) -> Option<String> {
    let params = ChainParams::default();
    match BlockTx::from_bytes_bounded(bytes, params.version, params.limits) {
        Ok(block_tx) => block_tx
            .tx
            .verify(LIMITS)
            .err()
            .map(|error| error.to_string()),
        Err(error) => Some(format!("the bytes do not decode: {error}")),
    }
}

/// The node's view of a contract, for a person.
async fn state_of(chain: &Chain, id: &[u8; 32]) -> Result<String, ChainError> {
    let states = chain.states(&[*id]).await?;
    Ok(states
        .first()
        .map(ContractState::to_string)
        .unwrap_or_else(|| "unknown to the node".to_owned()))
}
