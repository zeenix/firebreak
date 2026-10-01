//! Building, signing and packaging the three voucher transactions: funding, redemption and
//! recovery.
//!
//! Every builder returns an [`UnsignedTx`]. Its effect log is final before any signature exists,
//! so callers read the contracts it creates from [`UnsignedTx::log`] and store whatever they need
//! to recover them before they submit anything.

use curve25519_dalek::ristretto::CompressedRistretto;
use curve25519_dalek::scalar::Scalar as DalekScalar;
use flamechain::BlockTx;
use flamechain::utreexo::Proof;
use flamepayments::{Opening, PreparedOutput};
use flamevm::{
    Contract, ExternalTx, Limits, Scalar, ScriptBuilder, String as VmString, Token, TxEntry,
    TxHeader, TxLog, UnsignedTx, Value,
};
use merlin::Transcript;
use musig::{Multisignature, VerificationKey};

use crate::Error;
use crate::voucher::{
    BRANCH_GAS, RECEIPT_KEY, RECOVER_BRANCH, REDEEM_BRANCH, TOKEN_KEY, Voucher, VoucherPolicy,
};

/// The header of every Firebreak transaction.
pub const HEADER: TxHeader = TxHeader {
    version: 1,
    locktime: 0,
};

/// The gas budget of every Firebreak transaction.
pub const LIMITS: Limits = Limits { gas: 10_000_000 };

/// One wallet contract being spent through `signtx`, with its openings restored when its token is
/// confidential.
#[derive(Clone)]
pub struct WalletInput {
    contract: Contract,
    proof: Proof,
    signing_key: DalekScalar,
}

impl WalletInput {
    /// A cleartext input: the contract exactly as published, whose payload is a `ClearToken`.
    pub fn clear(
        published: Contract,
        proof: Proof,
        signing_key: DalekScalar,
    ) -> Result<WalletInput, Error> {
        if !matches!(published.payload(), Value::ClearToken(_)) {
            return Err(Error::NotAWalletOutput);
        }
        Ok(WalletInput {
            contract: published,
            proof,
            signing_key,
        })
    }

    /// A confidential input, rebuilt with open commitments from `opening`. Refused unless the
    /// rebuilt contract has the published contract's ID.
    pub fn confidential(
        published: &Contract,
        opening: &Opening,
        proof: Proof,
        signing_key: DalekScalar,
    ) -> Result<WalletInput, Error> {
        if !matches!(published.payload(), Value::Token(_)) {
            return Err(Error::NotAWalletOutput);
        }
        let token = Token::from_opening(
            Scalar::from(opening.qty),
            opening.flv,
            opening.qty_blinding,
            opening.flv_blinding,
        )
        .ok_or(Error::OpeningMismatch)?;
        let contract = Contract::new(
            published.predicate.to_opaque(),
            published.anchor,
            Value::Token(token),
        )?;
        if contract.id() != published.id() {
            return Err(Error::OpeningMismatch);
        }
        Ok(WalletInput {
            contract,
            proof,
            signing_key,
        })
    }

    /// The membership proof the chain checks for this input.
    pub fn proof(&self) -> &Proof {
        &self.proof
    }

    /// The key that authorizes this input.
    pub fn signing_key(&self) -> DalekScalar {
        self.signing_key
    }
}

/// One output of a funding transaction.
pub enum Payout<'a> {
    /// A voucher under `policy`, holding the prepared token and, as its receipt, the prepared
    /// note.
    Voucher {
        policy: &'a VoucherPolicy,
        prepared: &'a PreparedOutput,
    },
    /// An ordinary wallet output to the spending key `to`, followed by its note.
    Wallet {
        to: CompressedRistretto,
        prepared: &'a PreparedOutput,
    },
}

impl Payout<'_> {
    fn token(&self) -> &Token {
        match self {
            Payout::Voucher { prepared, .. } | Payout::Wallet { prepared, .. } => &prepared.token,
        }
    }
}

/// Builds the transaction that funds vouchers (and any change) from the owner's wallet inputs.
///
/// The script, in order:
///
/// ```text
/// per input:    push_str(String::contract(contract))  input  signtx
/// fee > 0:      push_int(fee)  fee
/// per payout:   push_str(commitment(qty))  push_str(commitment(flv))
///               push_int(m)  push_int(n)  mix
/// per payout i: roll_k(n-1-i) if > 0, then
///   voucher:    push_int(TOKEN_KEY)  push_str(receipt)  push_int(RECEIPT_KEY)  push_int(2)  dict
///               push_point(voucher predicate)  output
///   wallet:     push_point(S)  output  push_str(note)  log
/// ```
///
/// `mix` proves the inputs balance the payouts plus the fee, and range-proves every payout. The
/// payouts' commitments are exactly the prepared tokens', so each voucher's receipt describes its
/// token.
pub fn funding(
    inputs: &[WalletInput],
    payouts: &[Payout<'_>],
    fee: u64,
) -> Result<UnsignedTx, Error> {
    if payouts.is_empty() || payouts.len() > flamepayments::MAX_OUTPUTS {
        return Err(Error::PayoutCount(payouts.len()));
    }
    if inputs.is_empty() {
        return Err(Error::NoInputs);
    }

    let mut program = ScriptBuilder::new();
    for input in inputs {
        program = program
            .push_str(VmString::contract(input.contract.clone()))
            .input()
            .signtx();
    }
    let mut mix_inputs = inputs.len();
    if fee > 0 {
        program = program.push_int(fee).fee();
        mix_inputs += 1;
    }
    for payout in payouts {
        let token = payout.token();
        program = program
            .push_str(VmString::commitment(token.qty().clone()))
            .push_str(VmString::commitment(token.flv().clone()));
    }
    let count = payouts.len();
    program = program
        .push_int(mix_inputs as u64)
        .push_int(count as u64)
        .mix();

    for (index, payout) in payouts.iter().enumerate() {
        let depth = count - 1 - index;
        if depth > 0 {
            program = program.roll_k(depth as u8);
        }
        program = match payout {
            Payout::Voucher { policy, prepared } => program
                .push_int(TOKEN_KEY)
                .push_str(VmString::from(prepared.note.clone()))
                .push_int(RECEIPT_KEY)
                .push_int(2u64)
                .dict()
                .push_point(policy.predicate()?.to_bytes())
                .output(),
            Payout::Wallet { to, prepared } => program
                .push_point(to.to_bytes())
                .output()
                .push_str(VmString::from(prepared.note.clone()))
                .log(),
        };
    }

    Ok(program.build_tx(HEADER, LIMITS)?)
}

/// Builds the transaction that redeems `vouchers` to their merchant through the redemption
/// branch. Signed by the delegated key, once per voucher.
///
/// Per voucher, the script inputs the voucher, reveals its redemption program, opens it with no
/// arguments, and requires the program's success:
///
/// ```text
/// push_str(String::contract(voucher))  input
/// push_taproot_proof(tree, REDEEM_BRANCH)  push_int(BRANCH_GAS)  push_int(0)  open
/// verify  drop
/// ```
pub fn redemption(vouchers: &[&Voucher]) -> Result<UnsignedTx, Error> {
    if vouchers.is_empty() {
        return Err(Error::NoInputs);
    }
    let mut program = ScriptBuilder::new();
    for voucher in vouchers {
        program = open_branch(
            program,
            voucher.contract.clone(),
            &voucher.policy,
            REDEEM_BRANCH,
        )?;
    }
    Ok(program.build_tx(HEADER, LIMITS)?)
}

/// Builds the transaction that recovers `vouchers` into one fresh wallet output to the spending
/// key `to`. Signed by the owner's key, once per voucher.
///
/// Each voucher is input with its token's opening restored, and opened through the recovery
/// branch, which leaves its token on the stack. `mix` then turns the tokens into the prepared
/// output, which is followed by its note:
///
/// ```text
/// per voucher:  push_str(String::contract(voucher with opening))  input
///               push_taproot_proof(tree, RECOVER_BRANCH)  push_int(BRANCH_GAS)  push_int(0)  open
///               verify  drop
/// fee > 0:      push_int(fee)  fee
/// push_str(commitment(qty))  push_str(commitment(flv))  push_int(m)  push_int(1)  mix
/// push_point(S)  output  push_str(note)  log
/// ```
pub fn recovery(
    vouchers: &[(&Voucher, &Opening)],
    to: CompressedRistretto,
    prepared: &PreparedOutput,
    fee: u64,
) -> Result<UnsignedTx, Error> {
    if vouchers.is_empty() {
        return Err(Error::NoInputs);
    }
    let mut program = ScriptBuilder::new();
    for (voucher, opening) in vouchers {
        let contract = voucher.with_opening(opening)?;
        program = open_branch(program, contract, &voucher.policy, RECOVER_BRANCH)?;
    }
    let mut mix_inputs = vouchers.len();
    if fee > 0 {
        program = program.push_int(fee).fee();
        mix_inputs += 1;
    }
    let program = program
        .push_str(VmString::commitment(prepared.token.qty().clone()))
        .push_str(VmString::commitment(prepared.token.flv().clone()))
        .push_int(mix_inputs as u64)
        .push_int(1u64)
        .mix()
        .push_point(to.to_bytes())
        .output()
        .push_str(VmString::from(prepared.note.clone()))
        .log();
    Ok(program.build_tx(HEADER, LIMITS)?)
}

/// Appends: input `contract`, open its `branch` with no arguments, require success, and drop the
/// result count. Leaves the branch's results on the stack.
fn open_branch(
    program: ScriptBuilder,
    contract: Contract,
    policy: &VoucherPolicy,
    branch: usize,
) -> Result<ScriptBuilder, Error> {
    let tree = policy.tree()?;
    Ok(program
        .push_str(VmString::contract(contract))
        .input()
        .push_taproot_proof(&tree, branch)?
        .push_int(BRANCH_GAS)
        .push_int(0u64)
        .open()
        .verify()
        .drop_())
}

/// Signs `unsigned` with `keys`.
///
/// The VM records one signature requirement per `signtx`, each naming its verification key, and
/// the aggregate signature must cover them in that order, a key appearing once per requirement.
/// Each requirement is matched here to the key in `keys` that it names.
pub fn sign(unsigned: UnsignedTx, keys: &[DalekScalar]) -> Result<ExternalTx, Error> {
    let instructions = unsigned.signing_instructions();
    let secrets = instructions
        .items
        .iter()
        .map(|(vk, _)| {
            keys.iter()
                .find(|key| VerificationKey::from_secret(key).into_point() == *vk)
                .copied()
                .ok_or(Error::MissingKey(*vk))
        })
        .collect::<Result<Vec<_>, _>>()?;
    sign_positionally(unsigned, &secrets)
}

/// Signs `unsigned` with `secrets` matched to its signature requirements by position alone,
/// whatever keys those requirements name.
///
/// The honest path is [`sign`]. This is what an adversary holding the wrong keys does: the
/// signature it produces fails verification.
pub fn sign_positionally(
    unsigned: UnsignedTx,
    secrets: &[DalekScalar],
) -> Result<ExternalTx, Error> {
    let instructions = unsigned.signing_instructions();
    let mut transcript = Transcript::new(b"flamevm.signtx");
    transcript.append_message(b"txid", &instructions.txid.0);
    let items = instructions
        .items
        .iter()
        .map(|(vk, contract)| (VerificationKey::from_compressed(*vk), *contract))
        .collect();
    let signature = musig::Signature::sign_multi(secrets, items, &mut transcript)?;
    Ok(unsigned.sign(signature))
}

/// Packages a signed transaction for submission, with one membership proof per input in input
/// order.
pub fn package(tx: ExternalTx, proofs: Vec<Proof>) -> Result<Vec<u8>, Error> {
    let block_tx = BlockTx {
        tx,
        limits: LIMITS,
        proofs,
    };
    Ok(block_tx.to_bytes()?)
}

/// The contracts a transaction's effect log creates, each with the `Data` entry right after it.
pub fn outputs(log: &TxLog) -> Vec<(Contract, Option<Vec<u8>>)> {
    let entries = log.entries();
    entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| match entry {
            TxEntry::Output(contract) => {
                let data = match entries.get(index + 1) {
                    Some(TxEntry::Data(bytes)) => Some(bytes.clone()),
                    _ => None,
                };
                Some((contract.clone(), data))
            }
            _ => None,
        })
        .collect()
}

/// The contract IDs a transaction's effect log spends, in input order.
pub fn inputs(log: &TxLog) -> Vec<[u8; 32]> {
    log.entries()
        .iter()
        .filter_map(|entry| match entry {
            TxEntry::Input(id) => Some(*id),
            _ => None,
        })
        .collect()
}
