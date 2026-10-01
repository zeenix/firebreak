//! Voucher contracts: what a voucher locks, the two programs that can unlock it, and the predicate
//! tree that commits to exactly those two programs.
//!
//! A voucher is a Flame contract whose payload is a dictionary holding a confidential token and a
//! receipt: the note sealed, before funding, for the merchant output that the token will become.
//! Its predicate is a scripts-only Taproot tree whose internal key has no known discrete log, so
//! neither `signtx` nor `signcall` can ever unlock it directly. The tree holds two programs:
//!
//! * the redemption branch: the delegated key authorizes the transaction, and the program emits
//!   the whole token to the merchant's spending key, immediately followed by the receipt;
//! * the recovery branch: the owner's key authorizes the transaction, and the program discards
//!   the receipt and hands the token to that owner-signed transaction.
//!
//! Neither program takes arguments: the merchant, the amount and the receipt are all fixed when the
//! owner funds the voucher. A caller that passes arguments anyway leaves values on the program's
//! stack when it ends, which fails the branch and leaves the voucher locked.

use curve25519_dalek::ristretto::CompressedRistretto;
use flamekd::ReceivingAddress;
use flamepayments::Opening;
use flamevm::{
    Contract, Dict, Predicate, PredicateTree, Scalar, ScriptBuilder, String as VmString, Token,
    Value,
};

use crate::Error;

/// The payload key under which a voucher holds its token.
pub const TOKEN_KEY: u64 = 0;
/// The payload key under which a voucher holds the merchant's receipt.
pub const RECEIPT_KEY: u64 = 1;

/// The logical index of the redemption program in a voucher's predicate tree.
pub const REDEEM_BRANCH: usize = 0;
/// The logical index of the recovery program in a voucher's predicate tree.
pub const RECOVER_BRANCH: usize = 1;

/// The gas a transaction grants a voucher program when it opens one. Unused gas returns to the
/// transaction.
pub const BRANCH_GAS: u64 = 200_000;

/// Everything a voucher's predicate commits to.
///
/// Fixed by the owner when the voucher is funded. The delegate receives it too: it needs it to
/// rebuild the tree and reveal the redemption branch. None of it is secret from the delegate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoucherPolicy {
    /// The merchant. The redemption branch pays to its spending key, and the receipt is sealed to
    /// its viewing key.
    pub merchant: ReceivingAddress,
    /// The key that may redeem the voucher to the merchant.
    pub delegate: CompressedRistretto,
    /// The key that may recover the voucher.
    pub owner: CompressedRistretto,
    /// The seed of the tree's blinding leaves. Drawn fresh for every voucher, so two vouchers with
    /// the same parties still have unrelated predicates.
    pub blinding: [u8; 32],
}

impl VoucherPolicy {
    /// The predicate tree of a voucher under this policy.
    pub fn tree(&self) -> Result<PredicateTree, Error> {
        let programs = vec![self.redeem_program(), self.recover_program()];
        Ok(PredicateTree::from_scripts(None, programs, self.blinding)?)
    }

    /// The predicate point a voucher under this policy is locked by.
    pub fn predicate(&self) -> Result<CompressedRistretto, Error> {
        Ok(Predicate::tree(self.tree()?).to_point())
    }

    /// The merchant's spending key: the predicate of every payout this voucher can make.
    pub fn merchant_predicate(&self) -> CompressedRistretto {
        self.merchant.spending_key().compress()
    }

    /// The redemption program.
    ///
    /// It runs on the voucher's payload, `{TOKEN_KEY: token, RECEIPT_KEY: receipt}`, and in order:
    /// makes the delegated key authorize the transaction, outputs the token to the merchant's
    /// spending key, logs the receipt in the very next entry, and drops the emptied payload.
    pub fn redeem_program(&self) -> ScriptBuilder {
        authorize(ScriptBuilder::new(), &self.delegate)
            .push_int(TOKEN_KEY)
            .get()
            .roll_k(1)
            .drop_()
            .push_point(self.merchant_predicate().to_bytes())
            .output()
            .push_int(RECEIPT_KEY)
            .get()
            .roll_k(1)
            .drop_()
            .log()
            .drop_()
    }

    /// The recovery program.
    ///
    /// It runs on the voucher's payload, and in order: makes the owner's key authorize the
    /// transaction, drops the receipt, and returns the token to the transaction, which the owner's
    /// signature binds as a whole.
    pub fn recover_program(&self) -> ScriptBuilder {
        authorize(ScriptBuilder::new(), &self.owner)
            .push_int(RECEIPT_KEY)
            .get()
            .roll_k(1)
            .drop_()
            .drop_()
            .push_int(TOKEN_KEY)
            .get()
            .roll_k(1)
            .drop_()
            .roll_k(1)
            .drop_()
            .push_int(1u64)
            .return_()
    }
}

/// Appends the program fragment that makes `key` authorize the transaction from inside a voucher
/// program.
///
/// It locks a throwaway scalar under `key` and unlocks it with `signtx`, which records a
/// signature requirement bound to the transaction ID, then drops the scalar. The voucher's token
/// never leaves the program's control for it.
fn authorize(program: ScriptBuilder, key: &CompressedRistretto) -> ScriptBuilder {
    program
        .push_int(0u64)
        .push_point(key.to_bytes())
        .contract()
        .signtx()
        .drop_()
}

/// A voucher's payload: its token and the merchant's receipt.
pub fn payload(token: Token, receipt: Vec<u8>) -> Value {
    let mut dict = Dict::new();
    dict.insert(Scalar::from(TOKEN_KEY), Value::Token(token));
    dict.insert(
        Scalar::from(RECEIPT_KEY),
        Value::String(VmString::from(receipt)),
    );
    Value::Dict(dict)
}

/// A funded voucher, as its owner and its delegate know it.
#[derive(Clone, Debug)]
pub struct Voucher {
    /// What the voucher's predicate commits to.
    pub policy: VoucherPolicy,
    /// The voucher's contract as published.
    pub contract: Contract,
    /// The voucher's face value in sparks. Hidden from the public, known to the delegate.
    pub qty: u64,
}

impl Voucher {
    /// Checks that `contract` is locked under `policy`, and pairs them.
    pub fn new(policy: VoucherPolicy, contract: Contract, qty: u64) -> Result<Voucher, Error> {
        if contract.predicate.to_point() != policy.predicate()? {
            return Err(Error::PolicyMismatch);
        }
        receipt_of(&contract)?;
        Ok(Voucher {
            policy,
            contract,
            qty,
        })
    }

    /// The voucher's contract ID.
    pub fn id(&self) -> [u8; 32] {
        self.contract.id()
    }

    /// The receipt the voucher's redemption logs after its payout.
    pub fn receipt(&self) -> Result<Vec<u8>, Error> {
        receipt_of(&self.contract)
    }

    /// The voucher's contract with the token's opening restored, as the recovering transaction's
    /// prover needs it.
    ///
    /// Refused unless the rebuilt contract has the published contract's ID, which holds exactly
    /// when `opening` opens the published token.
    pub fn with_opening(&self, opening: &Opening) -> Result<Contract, Error> {
        let token = Token::from_opening(
            Scalar::from(opening.qty),
            opening.flv,
            opening.qty_blinding,
            opening.flv_blinding,
        )
        .ok_or(Error::OpeningMismatch)?;
        let contract = Contract::new(
            self.contract.predicate.to_opaque(),
            self.contract.anchor,
            payload(token, self.receipt()?),
        )?;
        if contract.id() != self.contract.id() {
            return Err(Error::OpeningMismatch);
        }
        Ok(contract)
    }
}

/// The receipt in a voucher contract's payload.
fn receipt_of(contract: &Contract) -> Result<Vec<u8>, Error> {
    let Value::Dict(dict) = contract.payload() else {
        return Err(Error::NotAVoucher);
    };
    if dict.len() != 2 || !matches!(dict.get(&Scalar::from(TOKEN_KEY)), Some(Value::Token(_))) {
        return Err(Error::NotAVoucher);
    }
    match dict.get(&Scalar::from(RECEIPT_KEY)) {
        Some(Value::String(receipt)) => Ok(receipt.to_bytes_vec()),
        _ => Err(Error::NotAVoucher),
    }
}
