//! Spending what was received: every unspent receipt moves to a fresh address of the merchant's
//! own, in one transaction that pays no fee. That shows the payments are ordinary money the
//! merchant controls, and leaves its balance exactly where it was.
//!
//! Like funding an allowance, spending is two steps, split where a crash or a lost answer would
//! split it. [`prepare`] builds the transaction and saves the spend as pending, which also saves
//! the fresh address as issued, so the funds are never paid to an address nobody looks at.
//! [`submit`] signs the transaction and offers it. [`spend`] does both.

use std::fmt;

use firebreak_core::build::{self, Payout, WalletInput};
use firebreak_core::chain::Chain;
use firebreak_core::store::{Progress, Spend};
use firebreak_core::wallet::OwnedOutput;
use firebreak_core::{ChainError, NETWORK};
use flamekd::ReceivingAddress;
use flamepayments::{Account, OutputSpec, PreparedOutput, prepare_output};
use flamevm::{FLAME_FLAVOR, TxID, UnsignedTx};
use rand::rngs::OsRng;

use crate::inspect::{self, is_receipt, observe};
use crate::offer::{self, Offer};
use crate::{Context, Error};

/// A spend that is built and saved as pending, and not yet offered to the node.
pub struct Prepared {
    txid: TxID,
    plan: Plan,
    built: Built,
}

impl Prepared {
    /// The spending transaction's id, which is known before anything is signed.
    pub fn txid(&self) -> TxID {
        self.txid
    }
}

/// What spending did.
#[derive(Debug)]
pub struct SpendReport {
    /// The spend that was submitted, or `None` when no receipt was unspent.
    pub spent: Option<Spent>,
    /// Things that went wrong without changing what the command achieved.
    pub warnings: Vec<String>,
}

impl fmt::Display for SpendReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut lines = Vec::new();
        match &self.spent {
            None => lines.push("nothing to spend: no payment received is unspent".to_owned()),
            Some(spent) => lines.push(format!(
                "spent {} sparks from {} payment(s) to the fresh address {} in transaction {} \
                 ({})",
                spent.qty,
                spent.receipts,
                spent.to.to_bech32(NETWORK),
                hex::encode(spent.txid.0),
                match spent.confirmed_height {
                    Some(height) => format!("confirmed in block {height}"),
                    None => "not confirmed yet".to_owned(),
                }
            )),
        }
        lines.extend(
            self.warnings
                .iter()
                .map(|warning| format!("warning: {warning}")),
        );
        write!(formatter, "{}", lines.join("\n"))
    }
}

/// A spend that was submitted.
#[derive(Debug)]
pub struct Spent {
    /// The spending transaction.
    pub txid: TxID,
    /// What it moved, in sparks, which is also what the merchant holds afterwards at the new
    /// address.
    pub qty: u64,
    /// The fresh address it paid.
    pub to: ReceivingAddress,
    /// How many payments it spent.
    pub receipts: usize,
    /// The block that confirmed it, when it was seen to confirm.
    pub confirmed_height: Option<u64>,
}

/// Spends every unspent payment to a fresh address: [`prepare`]s the spend and [`submit`]s it.
pub async fn spend(ctx: &Context, wait: bool) -> Result<SpendReport, Error> {
    match prepare(ctx).await? {
        Some(prepared) => submit(ctx, prepared, wait).await,
        None => Ok(SpendReport {
            spent: None,
            warnings: Vec::new(),
        }),
    }
}

/// Builds the transaction that spends every unspent payment and saves it as a pending spend,
/// with the fresh address it pays issued. `None` when no payment is unspent.
///
/// The merchant's spends are reconciled first, so a spend the node dropped is forgotten. A spend
/// that is still pending refuses a new one, which would spend the same payments again.
pub async fn prepare(ctx: &Context) -> Result<Option<Prepared>, Error> {
    let store = ctx.files.load()?;
    let observation = observe(ctx, &store).await?;
    inspect::settle(ctx, &observation)?;
    let store = ctx.files.load()?;
    if let Some(pending) = store
        .spends
        .iter()
        .find(|spend| spend.state == Progress::Pending)
    {
        return Err(Error::Refused(format!(
            "a spend is still pending in transaction {}; inspect again once it has confirmed",
            hex::encode(pending.txid.0)
        )));
    }

    let receipts: Vec<OwnedOutput> = observation
        .synced
        .outputs
        .into_iter()
        .filter(|output| is_receipt(output) && output.spent.is_none() && output.qty > 0)
        .collect();
    if receipts.is_empty() {
        return Ok(None);
    }
    let total = receipts
        .iter()
        .try_fold(0u64, |total, output| total.checked_add(output.qty))
        .ok_or_else(|| {
            Error::Refused("the payments add up to more than a transaction can hold".to_owned())
        })?;

    let account = store.account().map_err(Error::Wallet)?;
    let (_, to) = ctx
        .files
        .update(|store| store.next_address().map_err(Error::Wallet))?;
    let plan = Plan::new(account, receipts, to, total)?;
    let built = plan.build(&ctx.chain).await?;
    let txid = built.txid();
    ctx.files.update(|store| {
        store.spends.push(Spend {
            txid,
            qty: total,
            to,
            state: Progress::Pending,
        });
        Ok(())
    })?;
    Ok(Some(Prepared { txid, plan, built }))
}

/// Signs a prepared spend and offers it to the node, and waits for it to confirm if `wait`.
///
/// Every attempt is journaled. What the node answers decides what is saved:
///
/// * Accepted, the spend stays pending until a block confirms it.
/// * Refused, the spend is forgotten, and the error is the node's.
/// * No answer, the spend stays pending, and the error says that the outcome is unknown.
///   [`inspect`](crate::inspect) finds out.
///
/// A refusal for a proof that is no longer fresh is answered once by building the transaction
/// again around fresh proofs, which must be the same transaction.
pub async fn submit(ctx: &Context, prepared: Prepared, wait: bool) -> Result<SpendReport, Error> {
    let Prepared { txid, plan, built } = prepared;
    let (qty, to, receipts) = (plan.total, plan.to, plan.receipts.len());
    let mut warnings = Vec::new();
    let sent = match send(ctx, &plan, built, txid, &mut warnings).await {
        Ok(sent) => sent,
        Err(error) => {
            // The node refused it, so the spend never happened and moved nothing. If forgetting
            // it fails, the next inspection finds that the node does not know it.
            let _ = ctx.files.update(|store| {
                store.spends.retain(|spend| spend.txid != txid);
                Ok(())
            });
            let _ = inspect::inspect(ctx).await;
            return Err(error);
        }
    };
    // The spend is saved before it is offered. This makes sure it still is, whatever an
    // inspection that ran in between made of a transaction the node had not heard of yet. What
    // the node answered is already journaled, so a failure here is a warning and not an error: the
    // node holds the spend, and the next inspection brings the files in line.
    let recorded = ctx.files.update(|store| {
        if !store.spends.iter().any(|spend| spend.txid == txid) {
            store.spends.push(Spend {
                txid,
                qty,
                to,
                state: Progress::Pending,
            });
        }
        Ok(())
    });
    if let Sent::Unknown(cause) = sent {
        return Err(Error::Unknown {
            cause,
            advice: format!(
                "the spend {} may or may not have reached the node, and is saved as pending; run \
                 `firebreak-merchant inspect` to find out",
                hex::encode(txid.0)
            ),
        });
    }
    if let Err(error) = recorded {
        warnings.push(format!(
            "the node took the spend, but it could not be recorded: {error}; run \
             `firebreak-merchant inspect`"
        ));
    }

    let confirmed = if wait {
        Some(offer::confirm(ctx, &txid).await)
    } else {
        None
    };
    if let Err(error) = inspect::inspect(ctx).await {
        warnings.push(format!(
            "merchant-status.json was not brought up to date: {error}"
        ));
    }
    let confirmed_height = confirmed.transpose()?;
    Ok(SpendReport {
        spent: Some(Spent {
            txid,
            qty,
            to,
            receipts,
            confirmed_height,
        }),
        warnings,
    })
}

/// Everything about a spend that is chosen once: with it, the transaction can be built again
/// around fresh proofs and come out the same.
struct Plan {
    account: Account,
    /// The payments the spend moves.
    receipts: Vec<OwnedOutput>,
    /// The fresh address it pays.
    to: ReceivingAddress,
    /// What the payments add up to, in sparks.
    total: u64,
    /// The token and note that pay `total` to `to`.
    sealed: PreparedOutput,
}

impl Plan {
    /// The plan to move `receipts`, which add up to `total`, to the address `to`.
    fn new(
        account: Account,
        receipts: Vec<OwnedOutput>,
        to: ReceivingAddress,
        total: u64,
    ) -> Result<Plan, Error> {
        let spec = OutputSpec {
            address: to,
            qty: total,
            flv: FLAME_FLAVOR,
            memo: SPEND_MEMO.to_vec(),
        };
        let sealed =
            prepare_output(&spec, &mut OsRng).map_err(|error| Error::Prover(error.into()))?;
        Ok(Plan {
            account,
            receipts,
            to,
            total,
            sealed,
        })
    }

    /// Builds the spending transaction around proofs of the payments that are fresh now.
    async fn build(&self, chain: &Chain) -> Result<Built, Error> {
        let ids: Vec<[u8; 32]> = self.receipts.iter().map(|output| output.id).collect();
        let proofs = chain.fresh_proofs(&ids).await.map_err(Error::node)?;
        let inputs = self
            .receipts
            .iter()
            .zip(proofs)
            .map(|(output, proof)| output.to_input(&self.account, proof))
            .collect::<Result<Vec<_>, _>>()
            .map_err(Error::Prover)?;
        let payout = [Payout::Wallet {
            to: self.to.spending_key().compress(),
            prepared: &self.sealed,
        }];
        let unsigned = build::funding(&inputs, &payout, 0).map_err(Error::Prover)?;
        Ok(Built { unsigned, inputs })
    }
}

/// A spending transaction built around proofs that are fresh now.
struct Built {
    unsigned: UnsignedTx,
    inputs: Vec<WalletInput>,
}

impl Built {
    /// The transaction's id.
    fn txid(&self) -> TxID {
        self.unsigned.log().txid()
    }

    /// Signs the transaction with the keys of the payments it spends and packages it with their
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
}

/// What became of offering a transaction that the node did not refuse.
enum Sent {
    /// The node took the transaction into its mempool.
    Accepted,
    /// The node did not answer, so it may or may not have the transaction.
    Unknown(ChainError),
}

/// Offers the spending transaction to the node, and once more around fresh proofs if the node
/// refuses it for a proof that is no longer fresh.
async fn send(
    ctx: &Context,
    plan: &Plan,
    built: Built,
    txid: TxID,
    warnings: &mut Vec<String>,
) -> Result<Sent, Error> {
    let ids: Vec<[u8; 32]> = plan.receipts.iter().map(|output| output.id).collect();
    let note = format!("spend {} received payment(s) to a fresh address", ids.len());
    let offer = Offer {
        txid,
        inputs: &ids,
        note: &note,
    };

    let first = offer::offer(ctx, &offer, built.package()?).await;
    warnings.extend(first.warning);
    let mut answer = first.result;
    if let Err(error) = &answer
        && error.is_stale_proof()
    {
        // Another block has come since the proofs were taken. The transaction is the same with
        // fresh proofs; a refresh that finds a payment spent ends the attempt with the node's
        // word.
        let rebuilt = plan.build(&ctx.chain).await?;
        let found = rebuilt.txid();
        if found != txid {
            return Err(Error::Rebuilt {
                expected: hex::encode(txid.0),
                found: hex::encode(found.0),
            });
        }
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

/// The memo of the note that pays the merchant's own fresh address, which only the merchant
/// reads.
const SPEND_MEMO: &[u8] = b"firebreak spend";
