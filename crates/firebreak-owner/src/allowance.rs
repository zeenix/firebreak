//! Funding an allowance: choosing the wallet outputs it spends, building the funding transaction,
//! saving what recovers its vouchers, and offering the transaction to the node.
//!
//! Funding is two steps, split where a crash or a lost answer would split it. [`prepare`] builds
//! the transaction and saves the allowance with every opening, and has not yet contacted the node
//! to send anything. [`submit`] signs the transaction and offers it. [`create_allowance`] does
//! both.

use std::fmt;
use std::path::PathBuf;

use curve25519_dalek::ristretto::CompressedRistretto;
use firebreak_core::build::{self, Payout, WalletInput};
use firebreak_core::chain::Chain;
use firebreak_core::journal::Action;
use firebreak_core::store::serde_hex::HexBytes;
use firebreak_core::store::{self, OwnerAllowance, OwnerStore, VoucherState, allowance_id};
use firebreak_core::wallet::{self, OwnedOutput};
use firebreak_core::{ChainError, NETWORK, Voucher, VoucherPolicy, keys};
use flamekd::ReceivingAddress;
use flamepayments::{Account, Opening, OutputSpec, PreparedOutput, prepare_output};
use flamevm::{FLAME_FLAVOR, TxID, UnsignedTx};
use rand::rngs::OsRng;

use crate::offer::{self, Offer};
use crate::status::refresh;
use crate::{Context, Error};

/// The most outputs one funding transaction proves: the vouchers, and the change if there is any.
///
/// The prover's budget of multipliers runs out at the fourteenth output, however many wallet
/// outputs the transaction spends, so an allowance has 12 vouchers when its funding leaves
/// change, and 13 when the funding spends exactly what the vouchers add up to.
pub const MAX_PAYOUTS: usize = 13;

/// What to fund: an allowance of vouchers for one merchant, redeemable by one delegate.
#[derive(Clone, Debug)]
pub struct CreateAllowance {
    /// The merchant every voucher pays.
    pub merchant: ReceivingAddress,
    /// The verification key of the delegate, which redeems the vouchers.
    pub delegate: CompressedRistretto,
    /// The face value of each voucher, in sparks.
    pub vouchers: Vec<u64>,
    /// Whether to wait for the funding transaction to confirm.
    pub wait: bool,
}

/// The merchant address that `text` writes.
pub fn parse_merchant(text: &str) -> Result<ReceivingAddress, Error> {
    ReceivingAddress::from_bech32(text.trim(), NETWORK).map_err(|error| {
        Error::Refused(format!(
            "{text:?} is not a testnet merchant address (tf1...): {error}"
        ))
    })
}

/// The delegate's verification key that `text` writes as 64 hexadecimal digits.
///
/// The text is never repeated in an error: a person who pastes the wrong key may have pasted a
/// secret one.
pub fn parse_delegate(text: &str) -> Result<CompressedRistretto, Error> {
    let bytes = hex::decode(text.trim()).map_err(|_| {
        Error::Refused("the delegate key must be written as hexadecimal digits".to_owned())
    })?;
    let key = CompressedRistretto::from_raw(&bytes).map_err(|reason| {
        Error::Refused(format!(
            "the delegate key is not a verification key: {reason}"
        ))
    })?;
    // The identity is the verification key of the scalar zero, which everyone knows.
    if key.to_bytes() == [0; 32] {
        return Err(Error::Refused(
            "the delegate key is the identity, whose secret everyone knows".to_owned(),
        ));
    }
    Ok(key)
}

/// The voucher amounts that `text` lists, comma separated, such as `50,20,20,10`.
pub fn parse_vouchers(text: &str) -> Result<Vec<u64>, Error> {
    text.split(',')
        .map(|word| parse_amount(word.trim()))
        .collect()
}

/// An allowance that is built and saved, and not yet offered to the node.
///
/// Everything that recovers its vouchers is already in the owner's store, in the state
/// `prepared`.
pub struct Prepared {
    allowance: String,
    txid: TxID,
    plan: Plan,
    built: Built,
    wait: bool,
}

impl Prepared {
    /// The allowance's id.
    pub fn allowance(&self) -> &str {
        &self.allowance
    }

    /// The funding transaction's id, which is known before anything is signed.
    pub fn txid(&self) -> TxID {
        self.txid
    }
}

/// What funding an allowance did.
#[derive(Debug)]
pub struct AllowanceReport {
    /// The allowance's id.
    pub allowance: String,
    /// The funding transaction.
    pub funding_txid: TxID,
    /// The vouchers and where they stand.
    pub vouchers: Vec<VoucherLine>,
    /// The delegation package that was written for the delegate.
    pub package: PathBuf,
    /// The block that confirmed the funding transaction, when it was seen to confirm.
    pub confirmed_height: Option<u64>,
    /// Things that went wrong without changing what the command achieved.
    pub warnings: Vec<String>,
}

impl fmt::Display for AllowanceReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut lines = vec![format!("allowance {}", self.allowance)];
        lines.push(match self.confirmed_height {
            Some(height) => format!(
                "funding transaction {} (confirmed in block {height})",
                hex::encode(self.funding_txid.0)
            ),
            None => format!(
                "funding transaction {} (not confirmed yet)",
                hex::encode(self.funding_txid.0)
            ),
        });
        for voucher in &self.vouchers {
            lines.push(format!(
                "  voucher {}  {} sparks  {}",
                hex::encode(voucher.id),
                voucher.qty,
                voucher.state
            ));
        }
        lines.push(format!("delegation package {}", self.package.display()));
        lines.extend(
            self.warnings
                .iter()
                .map(|warning| format!("warning: {warning}")),
        );
        write!(formatter, "{}", lines.join("\n"))
    }
}

/// A voucher and where it stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoucherLine {
    /// The voucher's contract id.
    pub id: [u8; 32],
    /// The voucher's face value in sparks.
    pub qty: u64,
    /// Where the voucher stands, as last learned.
    pub state: VoucherState,
}

/// Funds an allowance: [`prepare`]s it and [`submit`]s it.
pub async fn create_allowance(
    ctx: &Context,
    request: &CreateAllowance,
) -> Result<AllowanceReport, Error> {
    let prepared = prepare(ctx, request).await?;
    submit(ctx, prepared).await
}

/// Builds the funding transaction of an allowance and saves the allowance, with the opening of
/// every voucher, in state `prepared`.
///
/// The wallet outputs that the allowance spends are chosen largest first from what the node says
/// is unspent. Nothing is saved for a request that is refused, and nothing that could fund a
/// voucher has been sent when this returns.
pub async fn prepare(ctx: &Context, request: &CreateAllowance) -> Result<Prepared, Error> {
    let store = ctx.files.load()?;
    let total = check(request, &store)?;
    let account = store.account().map_err(Error::Wallet)?;
    let synced = wallet::sync(
        &ctx.chain,
        &account,
        0..account.next_index(),
        store.change_range(),
    )
    .await
    .map_err(Error::sync)?;
    let (spend, change) = select_inputs(synced.outputs, total)?;
    check_payouts(request.vouchers.len(), change)?;
    let change = reserve_change(ctx, change)?;
    let plan = Plan::new(request, &store, account, spend, change)?;

    let built = plan.build(&ctx.chain).await?;
    let txid = built.txid();
    let pairs: Vec<(Voucher, Opening)> = built
        .vouchers
        .iter()
        .cloned()
        .zip(plan.sealed.iter().map(|sealed| sealed.opening))
        .collect();
    let record = OwnerAllowance::prepared(txid, &pairs).map_err(Error::Wallet)?;
    let allowance = record.allowance.clone();
    ctx.files.update(|store| {
        store.allowances.push(record);
        Ok(())
    })?;
    Ok(Prepared {
        allowance,
        txid,
        plan,
        built,
        wait: request.wait,
    })
}

/// Signs a prepared allowance's funding transaction and offers it to the node, and waits for it
/// to confirm if [`CreateAllowance::wait`] asked.
///
/// Every attempt is journaled. What the node answers decides what is saved:
///
/// * Accepted, the vouchers become `funding_pending` and the delegation package is written.
/// * Refused, the allowance stays `prepared`, and nothing is written for the delegate.
/// * No answer, the vouchers become `funding_pending` and the package is written, and the error
///   says that the outcome is unknown. [`status`](crate::status) finds out.
///
/// A refusal for a proof that is no longer fresh is answered once by building the transaction
/// again around fresh proofs. The result must be the same transaction, with the same vouchers.
pub async fn submit(ctx: &Context, prepared: Prepared) -> Result<AllowanceReport, Error> {
    let Prepared {
        allowance,
        txid,
        plan,
        built,
        wait,
    } = prepared;
    let mut warnings = Vec::new();
    let sent = match send(ctx, &plan, built, txid, &mut warnings).await {
        Ok(sent) => sent,
        Err(error) => {
            // The refusal is the answer; bringing the snapshot up to date is a courtesy.
            let _ = refresh(ctx).await;
            return Err(error);
        }
    };
    // What the node answered is already journaled. Recording it in the files is bookkeeping: if
    // it fails, the node still holds the funding, and `status` brings the files in line.
    let recorded = record_submission(ctx, &allowance);
    let package = match (sent, recorded) {
        (Sent::Accepted, Ok(package)) => package,
        (Sent::Accepted, Err(error)) => {
            warnings.push(format!(
                "the node took the funding, but it could not be recorded: {error}; run \
                 `firebreak-owner status`"
            ));
            ctx.files.package(&allowance)
        }
        (Sent::Unknown(cause), recorded) => {
            let state = if recorded.is_ok() {
                "funding_pending"
            } else {
                "prepared"
            };
            return Err(Error::Unknown {
                cause,
                advice: format!(
                    "the funding transaction {} may or may not have reached the node, and \
                     allowance {allowance} is saved with its vouchers {state}; run \
                     `firebreak-owner status` to find out",
                    hex::encode(txid.0)
                ),
            });
        }
    };

    let confirmed = if wait {
        Some(offer::confirm(ctx, &txid).await)
    } else {
        None
    };
    if let Err(error) = refresh(ctx).await {
        warnings.push(format!(
            "owner-status.json was not brought up to date: {error}"
        ));
    }
    let confirmed_height = confirmed.transpose()?;
    let vouchers = current_lines(ctx, &allowance, &mut warnings);
    Ok(AllowanceReport {
        vouchers,
        allowance,
        funding_txid: txid,
        package,
        confirmed_height,
        warnings,
    })
}

/// The vouchers of the allowance `allowance` as the owner's store has them now, and where they
/// stand.
///
/// This is for a report of something that has happened, so a store that cannot be read is a
/// warning and not an error: it does not undo what happened.
pub(crate) fn current_lines(
    ctx: &Context,
    allowance: &str,
    warnings: &mut Vec<String>,
) -> Vec<VoucherLine> {
    let store = match ctx.files.load() {
        Ok(store) => store,
        Err(error) => {
            warnings.push(format!("the vouchers' states could not be read: {error}"));
            return Vec::new();
        }
    };
    let Some(allowance) = store.allowance(allowance) else {
        return Vec::new();
    };
    allowance
        .vouchers
        .iter()
        .map(|voucher| VoucherLine {
            id: voucher.id,
            qty: voucher.qty,
            state: voucher.state,
        })
        .collect()
}

/// Everything about an allowance that is chosen once: with it, the funding transaction can be
/// built again around fresh proofs and come out the same.
struct Plan {
    account: Account,
    /// The wallet outputs the funding spends.
    spend: Vec<OwnedOutput>,
    policies: Vec<VoucherPolicy>,
    quantities: Vec<u64>,
    /// The vouchers' tokens and the receipts that go with them.
    sealed: Vec<PreparedOutput>,
    /// The wallet's change address and what is paid to it, unless nothing is left over.
    change: Option<Change>,
}

/// The change of a funding transaction.
struct Change {
    address: ReceivingAddress,
    sealed: PreparedOutput,
}

impl Plan {
    /// The plan for `request`: a policy and a sealed token for every voucher, and for the change
    /// at `change` if there is any, which are drawn fresh and never again.
    fn new(
        request: &CreateAllowance,
        store: &OwnerStore,
        account: Account,
        spend: Vec<OwnedOutput>,
        change: Option<(ReceivingAddress, u64)>,
    ) -> Result<Plan, Error> {
        let policies = request
            .vouchers
            .iter()
            .map(|_| VoucherPolicy {
                merchant: request.merchant,
                delegate: request.delegate,
                owner: store.authority(),
                blinding: keys::random_bytes(&mut OsRng),
            })
            .collect();
        let sealed = request
            .vouchers
            .iter()
            .map(|qty| seal(request.merchant, *qty, VOUCHER_MEMO))
            .collect::<Result<_, _>>()?;
        let change = match change {
            Some((address, qty)) => Some(Change {
                address,
                sealed: seal(address, qty, CHANGE_MEMO)?,
            }),
            None => None,
        };
        Ok(Plan {
            account,
            spend,
            policies,
            quantities: request.vouchers.clone(),
            sealed,
            change,
        })
    }

    /// Builds the funding transaction around proofs of the spent outputs that are fresh now.
    async fn build(&self, chain: &Chain) -> Result<Built, Error> {
        let spent: Vec<[u8; 32]> = self.spend.iter().map(|output| output.id).collect();
        let proofs = chain.fresh_proofs(&spent).await.map_err(Error::node)?;
        let inputs = self
            .spend
            .iter()
            .zip(proofs)
            .map(|(output, proof)| output.to_input(&self.account, proof))
            .collect::<Result<Vec<_>, _>>()
            .map_err(Error::Prover)?;

        let mut payouts: Vec<Payout<'_>> = self
            .policies
            .iter()
            .zip(&self.sealed)
            .map(|(policy, prepared)| Payout::Voucher { policy, prepared })
            .collect();
        if let Some(change) = &self.change {
            payouts.push(Payout::Wallet {
                to: change.address.spending_key().compress(),
                prepared: &change.sealed,
            });
        }
        let unsigned = build::funding(&inputs, &payouts, 0).map_err(Error::Prover)?;

        let created = build::outputs(unsigned.log());
        let vouchers = self
            .policies
            .iter()
            .zip(&self.quantities)
            .map(|(policy, qty)| {
                let predicate = policy.predicate().map_err(Error::Prover)?;
                let Some((contract, _)) = created
                    .iter()
                    .find(|(contract, _)| contract.predicate.to_point() == predicate)
                else {
                    return Err(Error::Prover(firebreak_core::Error::PolicyMismatch));
                };
                Voucher::new(*policy, contract.clone(), *qty).map_err(Error::Prover)
            })
            .collect::<Result<_, _>>()?;
        Ok(Built {
            unsigned,
            inputs,
            vouchers,
            spent,
        })
    }
}

/// A funding transaction built around proofs that are fresh now.
struct Built {
    unsigned: UnsignedTx,
    inputs: Vec<WalletInput>,
    /// The vouchers the transaction creates, in the order the allowance lists them.
    vouchers: Vec<Voucher>,
    /// The wallet outputs the transaction spends.
    spent: Vec<[u8; 32]>,
}

impl Built {
    /// The transaction's id.
    fn txid(&self) -> TxID {
        self.unsigned.log().txid()
    }

    /// Signs the transaction with the keys of the outputs it spends and packages it with their
    /// proofs, as the node takes it.
    fn package(self) -> Result<Vec<u8>, Error> {
        let keys: Vec<_> = self.inputs.iter().map(WalletInput::signing_key).collect();
        let proofs = self
            .inputs
            .iter()
            .map(|input| input.proof().clone())
            .collect();
        let tx = build::sign(self.unsigned, &keys).map_err(Error::Signer)?;
        build::package(tx, proofs).map_err(Error::Prover)
    }

    /// Checks that this is the transaction that was saved, creating the vouchers that were saved.
    fn check(&self, expected: TxID, vouchers: &[Voucher]) -> Result<(), Error> {
        let found = self.txid();
        let same_vouchers = self
            .vouchers
            .iter()
            .map(Voucher::id)
            .eq(vouchers.iter().map(Voucher::id));
        if found == expected && same_vouchers {
            return Ok(());
        }
        Err(Error::Rebuilt {
            expected: hex::encode(expected.0),
            found: hex::encode(found.0),
        })
    }
}

/// What became of offering a transaction that the node did not refuse.
enum Sent {
    /// The node took the transaction into its mempool.
    Accepted,
    /// The node did not answer, so it may or may not have the transaction.
    Unknown(ChainError),
}

/// Checks `request` against the owner's keys and returns the total of its vouchers.
fn check(request: &CreateAllowance, store: &OwnerStore) -> Result<u64, Error> {
    if request.delegate == store.authority() {
        return Err(Error::Refused(
            "the delegate key is the owner's own voucher authority key; the delegate must hold \
             a key of its own"
                .to_owned(),
        ));
    }
    if request.vouchers.is_empty() || request.vouchers.len() > MAX_PAYOUTS {
        return Err(Error::Refused(format!(
            "an allowance has 1 to {MAX_PAYOUTS} vouchers, not {}",
            request.vouchers.len()
        )));
    }
    if request.vouchers.contains(&0) {
        return Err(Error::Refused(
            "every voucher must be worth at least one spark".to_owned(),
        ));
    }
    request
        .vouchers
        .iter()
        .try_fold(0u64, |total, qty| total.checked_add(*qty))
        .ok_or_else(|| {
            Error::Refused("the vouchers add up to more than a transaction can hold".to_owned())
        })
}

/// The unspent wallet outputs that cover `needed`, largest first, and what they leave over.
fn select_inputs(outputs: Vec<OwnedOutput>, needed: u64) -> Result<(Vec<OwnedOutput>, u64), Error> {
    let mut unspent: Vec<OwnedOutput> = outputs
        .into_iter()
        .filter(|output| output.spent.is_none() && output.qty > 0)
        .collect();
    unspent.sort_by(|a, b| b.qty.cmp(&a.qty).then_with(|| a.id.cmp(&b.id)));

    let needed = u128::from(needed);
    let held: u128 = unspent.iter().map(|output| u128::from(output.qty)).sum();
    if held < needed {
        return Err(Error::Refused(format!(
            "insufficient funds: the wallet holds {held} sparks and the allowance needs {needed}"
        )));
    }
    let mut chosen = Vec::new();
    let mut gathered = 0u128;
    for output in unspent {
        if gathered >= needed {
            break;
        }
        gathered += u128::from(output.qty);
        chosen.push(output);
    }
    let change = u64::try_from(gathered - needed).map_err(|_| {
        Error::Refused(
            "the wallet outputs to spend add up to more than a transaction can hold".to_owned(),
        )
    })?;
    Ok((chosen, change))
}

/// Checks that the vouchers and the change fit in one funding transaction.
fn check_payouts(vouchers: usize, change: u64) -> Result<(), Error> {
    let payouts = vouchers + usize::from(change > 0);
    if payouts <= MAX_PAYOUTS {
        return Ok(());
    }
    Err(Error::Refused(format!(
        "{vouchers} vouchers and the change make {payouts} outputs, and one funding transaction \
         proves at most {MAX_PAYOUTS}; ask for at most {} vouchers",
        MAX_PAYOUTS - 1
    )))
}

/// Issues the change address that `change` sparks are paid to, and saves it as issued before
/// anything relies on it. Nothing is issued when nothing is left over.
fn reserve_change(ctx: &Context, change: u64) -> Result<Option<(ReceivingAddress, u64)>, Error> {
    if change == 0 {
        return Ok(None);
    }
    let (_, address) = ctx
        .files
        .update(|store| store.next_change_address().map_err(Error::Wallet))?;
    Ok(Some((address, change)))
}

/// Offers the funding transaction to the node, and once more around fresh proofs if the node
/// refuses it for a proof that is no longer fresh.
async fn send(
    ctx: &Context,
    plan: &Plan,
    built: Built,
    txid: TxID,
    warnings: &mut Vec<String>,
) -> Result<Sent, Error> {
    let saved = built.vouchers.clone();
    let spent = built.spent.clone();
    let note = format!(
        "fund allowance {} with {} vouchers",
        allowance_id(&txid),
        saved.len()
    );
    let offer = Offer {
        action: Action::Fund,
        txid,
        inputs: &spent,
        note: &note,
    };

    let first = offer::offer(ctx, &offer, built.package()?).await;
    warnings.extend(first.warning);
    let mut answer = first.result;
    if let Err(error) = &answer
        && error.is_stale_proof()
    {
        // Another block has come since the proofs were taken. The transaction is the same with
        // fresh proofs; a refresh that finds an input spent ends the attempt with the node's word.
        let rebuilt = plan.build(&ctx.chain).await?;
        rebuilt.check(txid, &saved)?;
        let second = offer::offer(ctx, &offer, rebuilt.package()?).await;
        warnings.extend(second.warning);
        answer = second.result;
        if let Err(error) = &answer
            && error.is_stale_proof()
        {
            return Err(Error::Contested(error.clone()));
        }
    }
    match answer {
        Ok(_) => Ok(Sent::Accepted),
        Err(error @ ChainError::Transport(_)) => Ok(Sent::Unknown(error)),
        Err(error) => Err(Error::Node(error)),
    }
}

/// Records that the funding transaction was offered: the vouchers are `funding_pending`, and the
/// delegation package is written for the delegate. Returns the package's path.
fn record_submission(ctx: &Context, allowance: &str) -> Result<PathBuf, Error> {
    let package = ctx.files.update(|store| {
        let Some(record) = store.allowance_mut(allowance) else {
            return Err(Error::Refused(format!(
                "allowance {allowance} is not in the owner's store"
            )));
        };
        for voucher in &mut record.vouchers {
            if voucher.state == VoucherState::Prepared {
                voucher.state = VoucherState::FundingPending;
            }
        }
        record.package().map_err(Error::Wallet)
    })?;
    let path = ctx.files.package(allowance);
    store::write_public(&path, &package)?;
    Ok(path)
}

/// The token of `qty` sparks for `address`, and the note that goes with it, sealed now so that
/// the funding transaction can publish both.
fn seal(address: ReceivingAddress, qty: u64, memo: &[u8]) -> Result<PreparedOutput, Error> {
    let spec = OutputSpec {
        address,
        qty,
        flv: FLAME_FLAVOR,
        memo: memo.to_vec(),
    };
    prepare_output(&spec, &mut OsRng).map_err(|error| Error::Prover(error.into()))
}

/// The amount that `word` writes, in sparks.
fn parse_amount(word: &str) -> Result<u64, Error> {
    let refused = || {
        Error::Refused(format!(
            "voucher amount {word:?} is not a positive whole number of sparks"
        ))
    };
    if word.is_empty() || !word.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(refused());
    }
    match word.parse::<u64>() {
        Ok(amount) if amount > 0 => Ok(amount),
        _ => Err(refused()),
    }
}

/// The memo of every voucher's receipt, which the merchant reads when it redeems.
const VOUCHER_MEMO: &[u8] = b"firebreak voucher";

/// The memo of the change output, which only the owner reads.
const CHANGE_MEMO: &[u8] = b"firebreak change";
