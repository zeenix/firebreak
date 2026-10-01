//! Adversarial transactions against Firebreak vouchers.
//!
//! The adversary here holds exactly what Firebreak's threat model gives it: the delegated private
//! key, the public voucher descriptors, and direct access to the node. It holds neither the owner's
//! key nor the merchant's. It builds every candidate with the VM's own script builder, not with
//! Firebreak's honest builders or the agent's policy checks, and signs each one as strongly as its
//! key allows: every signature requirement the VM records is signed with the delegated key.
//!
//! A candidate can be stopped at three places, and callers must report which one, never more:
//! the prover can refuse to prove it ([`Stopped::Prover`]), the signer can refuse to sign it
//! ([`Stopped::Signer`]), or the signed bytes can be refused by a verifier or the node. The last
//! one is the caller's to observe: [`craft`] only hands back the signed candidate.

use curve25519_dalek::ristretto::CompressedRistretto;
use curve25519_dalek::scalar::Scalar as DalekScalar;
use firebreak_core::build::{self, HEADER, LIMITS};
use firebreak_core::voucher::{
    BRANCH_GAS, RECEIPT_KEY, RECOVER_BRANCH, REDEEM_BRANCH, TOKEN_KEY, Voucher, VoucherPolicy,
};
use flamekd::ReceivingAddress;
use flamevm::{ExternalTx, PredicateTree, ScriptBuilder, String as VmString, UnsignedTx, VMError};

/// Every attack, in the order a demonstration runs them.
pub const ALL: [Attack; 10] = [
    Attack::KeyPath,
    Attack::OwnerBranch,
    Attack::ForgedLeaf,
    Attack::StrippedLeaf,
    Attack::ExtraArgument,
    Attack::MissingSignature,
    Attack::FeeSiphon,
    Attack::DuplicateInput,
    Attack::RedeemSpent,
    Attack::ReceiptSwap,
];

/// One way to try to take a voucher's value, or to misuse it, with the delegated key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attack {
    /// Unlock the voucher as if its predicate were a plain key, with `signtx`, and pay its token
    /// to the adversary.
    KeyPath,
    /// Open the owner's recovery branch and pay the token it returns to the adversary.
    OwnerBranch,
    /// Open the voucher with a redemption leaf that pays the adversary instead of the merchant.
    ForgedLeaf,
    /// Open the voucher with a redemption leaf that requires no authorization.
    StrippedLeaf,
    /// Open the genuine redemption branch with an extra argument, hoping it acts on that instead
    /// of the payload.
    ExtraArgument,
    /// Submit a genuine redemption with its signature stripped.
    MissingSignature,
    /// Pay a transaction fee out of a voucher's value.
    FeeSiphon,
    /// Redeem the same voucher twice in one transaction.
    DuplicateInput,
    /// Redeem a voucher that is no longer unspent, such as one the owner recovered.
    RedeemSpent,
    /// Redeem a voucher and log a forged receipt after it. A valid payment to the merchant: the
    /// point is that the merchant still reads the fixed receipt.
    ReceiptSwap,
}

impl Attack {
    /// The attack's command-line name.
    pub fn name(self) -> &'static str {
        match self {
            Attack::KeyPath => "key-path",
            Attack::OwnerBranch => "owner-branch",
            Attack::ForgedLeaf => "forged-leaf",
            Attack::StrippedLeaf => "stripped-leaf",
            Attack::ExtraArgument => "extra-argument",
            Attack::MissingSignature => "missing-signature",
            Attack::FeeSiphon => "fee-siphon",
            Attack::DuplicateInput => "duplicate-input",
            Attack::RedeemSpent => "redeem-spent",
            Attack::ReceiptSwap => "receipt-swap",
        }
    }

    /// The attack named `name`.
    pub fn from_name(name: &str) -> Option<Attack> {
        ALL.into_iter().find(|attack| attack.name() == name)
    }

    /// What the attack tries.
    pub fn description(self) -> &'static str {
        match self {
            Attack::KeyPath => {
                "Spend the voucher through `signtx` on its predicate, signed with the delegated \
                 key, and pay the token to the attacker."
            }
            Attack::OwnerBranch => {
                "Open the owner's recovery branch, sign its authorization with the delegated key, \
                 and pay the returned token to the attacker."
            }
            Attack::ForgedLeaf => {
                "Open the voucher with a redemption leaf identical to the real one except that it \
                 pays the attacker."
            }
            Attack::StrippedLeaf => {
                "Open the voucher with a redemption leaf that skips the delegated authorization."
            }
            Attack::ExtraArgument => {
                "Open the genuine redemption branch with an extra argument and require it to \
                 succeed."
            }
            Attack::MissingSignature => {
                "Submit a genuine redemption to the merchant with its signature stripped."
            }
            Attack::FeeSiphon => {
                "Redeem the voucher and pay a transaction fee in the same transaction, to be \
                 balanced out of the voucher's value."
            }
            Attack::DuplicateInput => "Redeem the same voucher twice in one transaction.",
            Attack::RedeemSpent => "Redeem a voucher whatever its state, e.g. after recovery.",
            Attack::ReceiptSwap => {
                "Redeem the voucher to the merchant and log a forged receipt after the real one. \
                 This is a valid payment: the merchant must still read the fixed receipt."
            }
        }
    }

    /// Whether the candidate is a valid transaction that the chain should accept. Only
    /// [`Attack::ReceiptSwap`] is: it pays the merchant the whole voucher, as any redemption does.
    pub fn is_valid_payment(self) -> bool {
        self == Attack::ReceiptSwap
    }
}

/// The adversary's keys and goals.
#[derive(Clone)]
pub struct Adversary {
    /// The delegated key the adversary obtained.
    pub delegate_key: DalekScalar,
    /// The adversary's own address, where it wants the vouchers' value.
    pub address: ReceivingAddress,
}

impl Adversary {
    /// The adversary's own spending key point: the predicate it pays itself to.
    pub fn destination(&self) -> CompressedRistretto {
        self.address.spending_key().compress()
    }
}

/// Where a candidate was stopped before any bytes existed.
#[derive(Debug, thiserror::Error)]
pub enum Stopped {
    #[error("the prover refused the candidate: {0}")]
    Prover(#[from] VMError),

    #[error("the signer refused the candidate: {0}")]
    Signer(firebreak_core::Error),

    #[error("the attack needs {needed} target voucher(s), got {got}")]
    Targets { needed: usize, got: usize },

    #[error("the target is not a usable voucher: {0}")]
    Target(firebreak_core::Error),
}

/// A signed adversarial transaction.
pub struct Candidate {
    /// The transaction, signed with the delegated key where it requires signatures.
    pub tx: ExternalTx,
    /// The contracts it spends, in input order. The caller attaches their membership proofs.
    pub inputs: Vec<[u8; 32]>,
}

/// Builds and signs `attack` against `targets`.
pub fn craft(
    attack: Attack,
    targets: &[&Voucher],
    adversary: &Adversary,
) -> Result<Candidate, Stopped> {
    let target = *targets
        .first()
        .ok_or(Stopped::Targets { needed: 1, got: 0 })?;
    let inputs = vec![target.id()];
    let program = match attack {
        Attack::KeyPath => key_path(target, adversary),
        Attack::OwnerBranch => open(input(target), &target.policy.tree()?, RECOVER_BRANCH)?
            .push_point(adversary.destination().to_bytes())
            .output(),
        Attack::ForgedLeaf => {
            let forged = VoucherPolicy {
                merchant: adversary.address,
                ..target.policy
            };
            open(input(target), &forged.tree()?, REDEEM_BRANCH)?
        }
        Attack::StrippedLeaf => {
            let tree = PredicateTree::from_scripts(
                None,
                vec![
                    stripped_redeem(&target.policy),
                    target.policy.recover_program(),
                ],
                target.policy.blinding,
            )?;
            open(input(target), &tree, REDEEM_BRANCH)?
        }
        Attack::ExtraArgument => {
            // The decoy is shaped like a payload, `{TOKEN_KEY: 1, RECEIPT_KEY: "decoy"}`, so the
            // branch can run every instruction on it and only meet the real payload at its end.
            input(target)
                .push_taproot_proof(&target.policy.tree()?, REDEEM_BRANCH)?
                .push_int(BRANCH_GAS)
                .push_int(1u64)
                .push_int(TOKEN_KEY)
                .push_str(VmString::from(b"decoy".to_vec()))
                .push_int(RECEIPT_KEY)
                .push_int(2u64)
                .dict()
                .push_int(1u64)
                .open()
                .verify()
                .drop_()
        }
        Attack::MissingSignature => {
            // `UnsignedTx::without_signature` refuses a transaction that requires signatures, so
            // the adversary signs it and then strips the signature from the public struct.
            let mut tx = sign(redeem(&[target])?, adversary)?;
            tx.signature = None;
            return Ok(Candidate { tx, inputs });
        }
        Attack::FeeSiphon => open(input(target), &target.policy.tree()?, REDEEM_BRANCH)?
            .push_int(1u64)
            .fee(),
        Attack::DuplicateInput => {
            let tx = sign(redeem(&[target, target])?, adversary)?;
            return Ok(Candidate {
                tx,
                inputs: vec![target.id(), target.id()],
            });
        }
        Attack::RedeemSpent => {
            let tx = sign(redeem(&[target])?, adversary)?;
            return Ok(Candidate { tx, inputs });
        }
        Attack::ReceiptSwap => open(input(target), &target.policy.tree()?, REDEEM_BRANCH)?
            .push_str(VmString::from(forged_receipt(target)?))
            .log(),
    };
    let unsigned = program.build_tx(HEADER, LIMITS)?;
    Ok(Candidate {
        tx: sign(unsigned, adversary)?,
        inputs,
    })
}

/// The bytes the forged receipt of [`Attack::ReceiptSwap`] carries: the real receipt with its
/// last byte flipped, so it is the right length but authenticates under no key.
pub fn forged_receipt(target: &Voucher) -> Result<Vec<u8>, Stopped> {
    let mut receipt = target.receipt()?;
    if let Some(last) = receipt.last_mut() {
        *last ^= 0x01;
    }
    Ok(receipt)
}

/// Signs every signature requirement of `unsigned` with the delegated key, whatever key the
/// requirement names.
fn sign(unsigned: UnsignedTx, adversary: &Adversary) -> Result<ExternalTx, Stopped> {
    let count = unsigned.signing_instructions().items.len();
    if count == 0 {
        return Ok(unsigned.without_signature()?);
    }
    Ok(build::sign_positionally(
        unsigned,
        &vec![adversary.delegate_key; count],
    )?)
}

/// `input` of the voucher, with its public contract as the witness.
fn input(target: &Voucher) -> ScriptBuilder {
    ScriptBuilder::new()
        .push_str(VmString::contract(target.contract.clone()))
        .input()
}

/// Appends: open `branch` of `tree` with no arguments, require success, drop the result count.
fn open(
    program: ScriptBuilder,
    tree: &PredicateTree,
    branch: usize,
) -> Result<ScriptBuilder, VMError> {
    Ok(program
        .push_taproot_proof(tree, branch)?
        .push_int(BRANCH_GAS)
        .push_int(0u64)
        .open()
        .verify()
        .drop_())
}

/// The genuine redemption of `targets`, assembled here rather than by the honest builder.
fn redeem(targets: &[&Voucher]) -> Result<UnsignedTx, Stopped> {
    let mut program = ScriptBuilder::new();
    for target in targets {
        let tree = target.policy.tree()?;
        program = program
            .push_str(VmString::contract(target.contract.clone()))
            .input()
            .push_taproot_proof(&tree, REDEEM_BRANCH)?
            .push_int(BRANCH_GAS)
            .push_int(0u64)
            .open()
            .verify()
            .drop_();
    }
    Ok(program.build_tx(HEADER, LIMITS)?)
}

/// `signtx` the voucher itself, then pay its token to the adversary and drop the receipt.
fn key_path(target: &Voucher, adversary: &Adversary) -> ScriptBuilder {
    input(target)
        .signtx()
        .push_int(TOKEN_KEY)
        .get()
        .roll_k(1)
        .drop_()
        .push_point(adversary.destination().to_bytes())
        .output()
        .push_int(RECEIPT_KEY)
        .get()
        .roll_k(1)
        .drop_()
        .drop_()
        .drop_()
}

/// The genuine redemption program without its authorization fragment.
fn stripped_redeem(policy: &VoucherPolicy) -> ScriptBuilder {
    ScriptBuilder::new()
        .push_int(TOKEN_KEY)
        .get()
        .roll_k(1)
        .drop_()
        .push_point(policy.merchant_predicate().to_bytes())
        .output()
        .push_int(RECEIPT_KEY)
        .get()
        .roll_k(1)
        .drop_()
        .log()
        .drop_()
}

impl From<firebreak_core::Error> for Stopped {
    fn from(error: firebreak_core::Error) -> Stopped {
        match error {
            firebreak_core::Error::Vm(error) => Stopped::Prover(error),
            error @ (firebreak_core::Error::Musig(_) | firebreak_core::Error::MissingKey(_)) => {
                Stopped::Signer(error)
            }
            other => Stopped::Target(other),
        }
    }
}
