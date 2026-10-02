//! Taking vouchers back: recovering into the wallet every selected voucher that the node says is
//! still unspent.
//!
//! The delegate can redeem a voucher until the owner's recovery of it is in a block, and the
//! other way round, so a recovery is only ever a request. A voucher is called revoked once its
//! recovery has confirmed, and not before.

use std::fmt;

use curve25519_dalek::ristretto::CompressedRistretto;
use curve25519_dalek::scalar::Scalar;
use firebreak_core::build;
use firebreak_core::journal::Action;
use firebreak_core::store::{OwnerStore, OwnerVoucher, Progress, Recovery, VoucherState};
use firebreak_core::{ChainError, Voucher};
use flamepayments::{Opening, OutputSpec, PreparedOutput, prepare_output};
use flamevm::{FLAME_FLAVOR, TxID, UnsignedTx};
use rand::rngs::OsRng;

use crate::allowance::{VoucherLine, current_lines};
use crate::offer::{self, Offer, Offered};
use crate::status::refresh;
use crate::{Context, Error};

/// Which vouchers to recover.
#[derive(Clone, Debug, Default)]
pub struct Reclaim {
    /// Only the vouchers of this allowance, by its id, or every allowance's when there is none.
    pub allowance: Option<String>,
    /// Only these vouchers, each by its contract id or by the first digits of it that name it
    /// alone, or every voucher when there are none.
    pub vouchers: Vec<String>,
    /// Whether to wait for each recovery transaction to confirm.
    pub wait: bool,
}

/// What reclaiming did.
#[derive(Debug)]
pub struct ReclaimReport {
    /// The recovery transactions that were submitted, one for each allowance that had vouchers
    /// to recover.
    pub recoveries: Vec<Recovered>,
    /// The selected vouchers that were left alone, and why.
    pub skipped: Vec<Skip>,
    /// Things that went wrong without changing what the command achieved.
    pub warnings: Vec<String>,
}

impl fmt::Display for ReclaimReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut lines = Vec::new();
        for recovery in &self.recoveries {
            let txid = hex::encode(recovery.txid.0);
            let count = recovery.vouchers.len();
            lines.push(match (recovery.is_revoked(), recovery.confirmed_height) {
                (true, Some(height)) => format!(
                    "revoked: transaction {txid} confirmed in block {height}, and {count} \
                     voucher(s) of allowance {} are back in the wallet",
                    recovery.allowance
                ),
                (true, None) => format!(
                    "revoked: transaction {txid} confirmed, and {count} voucher(s) of allowance \
                     {} are back in the wallet",
                    recovery.allowance
                ),
                (false, _) => format!(
                    "recovery pending: transaction {txid} recovers {count} voucher(s) of \
                     allowance {}; they are revoked only once it confirms (wait for it with \
                     --wait, or see `firebreak-owner status`)",
                    recovery.allowance
                ),
            });
            for voucher in &recovery.vouchers {
                lines.push(format!(
                    "  voucher {}  {} sparks  {}",
                    hex::encode(voucher.id),
                    voucher.qty,
                    voucher.state
                ));
            }
        }
        for skip in &self.skipped {
            lines.push(format!(
                "left alone: voucher {}  {} sparks: {}",
                hex::encode(skip.id),
                skip.qty,
                skip.reason
            ));
        }
        if self.recoveries.is_empty() {
            lines.push("nothing to recover".to_owned());
        }
        lines.extend(
            self.warnings
                .iter()
                .map(|warning| format!("warning: {warning}")),
        );
        write!(formatter, "{}", lines.join("\n"))
    }
}

/// One recovery transaction that was submitted.
#[derive(Debug)]
pub struct Recovered {
    /// The allowance whose vouchers it recovers.
    pub allowance: String,
    /// The recovery transaction.
    pub txid: TxID,
    /// The vouchers it recovers, and where they stand.
    pub vouchers: Vec<VoucherLine>,
    /// The block that confirmed it, when it was seen to confirm.
    pub confirmed_height: Option<u64>,
}

impl Recovered {
    /// Whether the recovery has confirmed, which is what makes its vouchers revoked.
    pub fn is_revoked(&self) -> bool {
        self.confirmed_height.is_some()
            || self
                .vouchers
                .iter()
                .all(|voucher| voucher.state == VoucherState::Recovered)
    }
}

/// A selected voucher that was left alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skip {
    /// The voucher's contract id.
    pub id: [u8; 32],
    /// The voucher's face value in sparks.
    pub qty: u64,
    /// Why it was left alone.
    pub reason: String,
}

/// Recovers the selected vouchers that the node says are unspent, one transaction for each
/// allowance, paying a fresh change address of the wallet.
///
/// The vouchers' states are reconciled with the node first. The recovery is saved as
/// `recovery_pending` before it is offered, and a voucher that another transaction has taken in
/// the meantime is left alone while the others are recovered. Every attempt is journaled. When
/// the node refuses a recovery, the vouchers are unspent again and the error is the node's.
pub async fn reclaim(ctx: &Context, request: &Reclaim) -> Result<ReclaimReport, Error> {
    refresh(ctx).await?;
    let store = ctx.files.load()?;
    let selection = select(&store, request)?;

    let mut work = Work {
        submitted: Vec::new(),
        skipped: selection.skipped,
        warnings: Vec::new(),
    };
    let mut failure = None;
    for batch in selection.batches {
        if let Err(error) = recover_batch(ctx, batch, request.wait, &mut work).await {
            failure = Some(error);
            break;
        }
    }
    // A node that did not answer is not asked again at once; any other end brings the snapshot
    // up to date.
    let refreshed = match failure {
        Some(Error::Unknown { .. }) => None,
        _ => Some(refresh(ctx).await),
    };
    if let Some(error) = failure {
        return Err(error);
    }
    if let Some(Err(error)) = refreshed {
        work.warnings.push(format!(
            "owner-status.json was not brought up to date: {error}"
        ));
    }

    let mut warnings = work.warnings;
    let recoveries = work
        .submitted
        .into_iter()
        .map(|submitted| {
            let lines = current_lines(ctx, &submitted.allowance, &mut warnings);
            Recovered {
                vouchers: lines
                    .into_iter()
                    .filter(|line| submitted.vouchers.contains(&line.id))
                    .collect(),
                allowance: submitted.allowance,
                txid: submitted.txid,
                confirmed_height: submitted.confirmed_height,
            }
        })
        .collect();
    Ok(ReclaimReport {
        recoveries,
        skipped: work.skipped,
        warnings,
    })
}

/// The vouchers of `request`'s selection that can be recovered, and those that cannot.
fn select(store: &OwnerStore, request: &Reclaim) -> Result<Selection, Error> {
    let allowances = match &request.allowance {
        Some(id) => {
            let Some(allowance) = store.allowance(id) else {
                return Err(Error::Refused(format!("there is no allowance {id}")));
            };
            vec![allowance]
        }
        None => store.allowances.iter().collect(),
    };
    let named = named_vouchers(&allowances, &request.vouchers)?;

    let mut selection = Selection {
        batches: Vec::new(),
        skipped: Vec::new(),
    };
    for allowance in allowances {
        let mut batch = Batch {
            allowance: allowance.allowance.clone(),
            vouchers: Vec::new(),
        };
        for voucher in &allowance.vouchers {
            if named
                .as_ref()
                .is_some_and(|named| !named.contains(&voucher.id))
            {
                continue;
            }
            if voucher.state == VoucherState::Unspent {
                batch.vouchers.push(voucher.id);
            } else {
                selection.skipped.push(Skip {
                    id: voucher.id,
                    qty: voucher.qty,
                    reason: why_not(voucher),
                });
            }
        }
        if !batch.vouchers.is_empty() {
            selection.batches.push(batch);
        }
    }
    Ok(selection)
}

/// The vouchers a request selects, and those it names that cannot be recovered.
struct Selection {
    batches: Vec<Batch>,
    skipped: Vec<Skip>,
}

/// The vouchers of one allowance that are to be recovered together.
struct Batch {
    allowance: String,
    vouchers: Vec<[u8; 32]>,
}

/// The ids of the vouchers that `names` name, or `None` when there are no names and every
/// voucher is meant.
fn named_vouchers(
    allowances: &[&firebreak_core::store::OwnerAllowance],
    names: &[String],
) -> Result<Option<Vec<[u8; 32]>>, Error> {
    if names.is_empty() {
        return Ok(None);
    }
    let mut ids = Vec::new();
    for name in names {
        let prefix = name.trim().to_ascii_lowercase();
        if prefix.is_empty() || !prefix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(Error::Refused(format!("{name:?} is not a voucher id")));
        }
        let mut matching = allowances
            .iter()
            .flat_map(|allowance| allowance.vouchers.iter())
            .filter(|voucher| hex::encode(voucher.id).starts_with(&prefix));
        match (matching.next(), matching.next()) {
            (Some(voucher), None) => ids.push(voucher.id),
            (None, _) => {
                return Err(Error::Refused(format!(
                    "no voucher has an id starting {name:?}"
                )));
            }
            (Some(_), Some(_)) => {
                return Err(Error::Refused(format!(
                    "more than one voucher has an id starting {name:?}; write more of it"
                )));
            }
        }
    }
    Ok(Some(ids))
}

/// Why a voucher that is not unspent is left alone.
fn why_not(voucher: &OwnerVoucher) -> String {
    match voucher.state {
        VoucherState::RecoveryPending => match voucher.txid {
            Some(txid) => format!(
                "a recovery is already pending in transaction {}",
                hex::encode(txid.0)
            ),
            None => "a recovery is already pending".to_owned(),
        },
        VoucherState::Recovered => "it was already recovered".to_owned(),
        VoucherState::Redeemed => "the delegate already redeemed it".to_owned(),
        VoucherState::Prepared => {
            "it was never funded: the node does not know its funding transaction".to_owned()
        }
        VoucherState::FundingPending => "its funding transaction is not confirmed yet".to_owned(),
        VoucherState::Unknown => "the node does not know it".to_owned(),
        other => format!("it is {other}, which recovery does not take"),
    }
}

/// What a reclaim has done so far.
struct Work {
    submitted: Vec<Submitted>,
    skipped: Vec<Skip>,
    warnings: Vec<String>,
}

/// A recovery transaction that was accepted, or may have been.
struct Submitted {
    allowance: String,
    txid: TxID,
    vouchers: Vec<[u8; 32]>,
    confirmed_height: Option<u64>,
}

/// Recovers the vouchers of `batch`, dropping any that another transaction has taken, and waits
/// for the confirmation if asked.
async fn recover_batch(
    ctx: &Context,
    batch: Batch,
    wait: bool,
    work: &mut Work,
) -> Result<(), Error> {
    let Batch {
        allowance,
        mut vouchers,
    } = batch;
    // Every lost voucher leaves the batch, so this ends.
    while !vouchers.is_empty() {
        match recover_once(ctx, &allowance, &vouchers, work).await? {
            Attempt::Submitted(txid) => {
                let waited = if wait {
                    Some(offer::confirm(ctx, &txid).await)
                } else {
                    None
                };
                work.submitted.push(Submitted {
                    allowance,
                    txid,
                    vouchers,
                    confirmed_height: waited.transpose()?,
                });
                return Ok(());
            }
            Attempt::Lost(skip) => {
                vouchers.retain(|voucher| *voucher != skip.id);
                work.skipped.push(skip);
            }
        }
    }
    Ok(())
}

/// What became of one attempt to recover a batch.
enum Attempt {
    /// The node took the recovery transaction.
    Submitted(TxID),
    /// A voucher turned out to be spent, so it was dropped from the batch and nothing was sent.
    Lost(Skip),
}

/// Builds, saves and offers one recovery transaction for the vouchers `ids`.
///
/// The vouchers are `recovery_pending`, with the transaction's id, before the node is asked. What
/// the node answers decides what stays: its refusal undoes it, and silence leaves it, since the
/// node may have the transaction.
async fn recover_once(
    ctx: &Context,
    allowance: &str,
    ids: &[[u8; 32]],
    work: &mut Work,
) -> Result<Attempt, Error> {
    let plan = Plan::new(ctx, allowance, ids)?;
    let unsigned = plan.build()?;
    let txid = unsigned.log().txid();
    let voucher_ids = plan.ids();
    ctx.files
        .update(|store| mark_pending(store, allowance, &voucher_ids, txid))?;

    match send(ctx, &plan, unsigned, txid, &mut work.warnings).await {
        Ok(Sent::Accepted) => {
            // The node took it and the journal says so. A reconciliation that ran in between may
            // have forgotten the recovery, so this makes sure the files still know it.
            let recorded = ctx
                .files
                .update(|store| mark_pending(store, allowance, &voucher_ids, txid));
            if let Err(error) = recorded {
                work.warnings.push(format!(
                    "the node took the recovery, but it could not be recorded: {error}; run \
                     `firebreak-owner status`"
                ));
            }
            Ok(Attempt::Submitted(txid))
        }
        Ok(Sent::Unknown(cause)) => {
            // The vouchers were saved as pending before the node was asked, and stay so. This
            // only makes sure of it, so a failure changes nothing.
            let _ = ctx
                .files
                .update(|store| mark_pending(store, allowance, &voucher_ids, txid));
            Err(Error::Unknown {
                cause,
                advice: format!(
                    "the recovery transaction {} may or may not have reached the node, and its \
                     vouchers are saved as recovery_pending; run `firebreak-owner status` to \
                     find out",
                    hex::encode(txid.0)
                ),
            })
        }
        Err(failure) => {
            // Nothing was taken. If this write fails too, the next reconciliation finds the node
            // does not know the transaction and undoes it.
            let _ = ctx.files.update(|store| {
                unmark_pending(store, allowance, txid);
                Ok(())
            });
            match failure {
                Failure::Lost { voucher, state } => Ok(Attempt::Lost(lost(&plan, voucher, &state))),
                Failure::Error(error) => Err(error),
            }
        }
    }
}

/// Everything about one recovery that is chosen once: with it, the recovery transaction can be
/// built again and come out the same.
struct Plan {
    allowance: String,
    /// The vouchers to recover, with the openings of their tokens.
    vouchers: Vec<(Voucher, Opening)>,
    /// The key that authorizes recovery.
    authority_key: Scalar,
    /// The wallet's fresh change address, as a spending key.
    to: CompressedRistretto,
    /// The token and note the vouchers' value is paid back as.
    sealed: PreparedOutput,
}

impl Plan {
    /// The plan to recover the vouchers `ids` of `allowance`, paying them to a change address
    /// that is issued, and saved as issued, now.
    fn new(ctx: &Context, allowance: &str, ids: &[[u8; 32]]) -> Result<Plan, Error> {
        let store = ctx.files.load()?;
        let Some(record) = store.allowance(allowance) else {
            return Err(Error::Refused(format!(
                "allowance {allowance} is not in the owner's store"
            )));
        };
        let rebuilt = record.vouchers().map_err(Error::Wallet)?;
        let vouchers: Vec<(Voucher, Opening)> = rebuilt
            .into_iter()
            .zip(&record.vouchers)
            .filter(|(voucher, _)| ids.contains(&voucher.id()))
            .map(|(voucher, stored)| (voucher, stored.opening))
            .collect();
        let total = vouchers
            .iter()
            .try_fold(0u64, |total, (voucher, _)| total.checked_add(voucher.qty))
            .ok_or_else(|| {
                Error::Refused("the vouchers add up to more than a transaction can hold".to_owned())
            })?;

        let (_, address) = ctx
            .files
            .update(|store| store.next_change_address().map_err(Error::Wallet))?;
        let spec = OutputSpec {
            address,
            qty: total,
            flv: FLAME_FLAVOR,
            memo: RECOVERY_MEMO.to_vec(),
        };
        let sealed =
            prepare_output(&spec, &mut OsRng).map_err(|error| Error::Prover(error.into()))?;
        Ok(Plan {
            allowance: allowance.to_owned(),
            vouchers,
            authority_key: store.authority_key,
            to: address.spending_key().compress(),
            sealed,
        })
    }

    /// The contract ids of the vouchers, in the order of the transaction's inputs.
    fn ids(&self) -> Vec<[u8; 32]> {
        self.vouchers
            .iter()
            .map(|(voucher, _)| voucher.id())
            .collect()
    }

    /// Builds the recovery transaction.
    fn build(&self) -> Result<UnsignedTx, Error> {
        let pairs: Vec<(&Voucher, &Opening)> = self
            .vouchers
            .iter()
            .map(|(voucher, opening)| (voucher, opening))
            .collect();
        build::recovery(&pairs, self.to, &self.sealed, 0).map_err(Error::Prover)
    }
}

/// Why a recovery transaction was not offered or was refused.
enum Failure {
    /// The node says that this voucher is no longer unspent.
    Lost { voucher: [u8; 32], state: String },
    /// Anything else.
    Error(Error),
}

impl From<Error> for Failure {
    fn from(error: Error) -> Failure {
        Failure::Error(error)
    }
}

/// What became of offering a recovery transaction that the node did not refuse.
enum Sent {
    /// The node took the transaction into its mempool.
    Accepted,
    /// The node did not answer, so it may or may not have the transaction.
    Unknown(ChainError),
}

/// Offers the recovery transaction to the node, and once more if the node refuses it for a proof
/// that is no longer fresh. A voucher that has no proof to refresh has been spent.
async fn send(
    ctx: &Context,
    plan: &Plan,
    unsigned: UnsignedTx,
    txid: TxID,
    warnings: &mut Vec<String>,
) -> Result<Sent, Failure> {
    let ids = plan.ids();
    let note = format!(
        "recover {} vouchers of allowance {}",
        ids.len(),
        plan.allowance
    );
    let offer = Offer {
        action: Action::Recover,
        txid,
        inputs: &ids,
        note: &note,
    };

    let first = offer_fresh(ctx, plan, unsigned, &offer).await?;
    warnings.extend(first.warning);
    let mut answer = first.result;
    if let Err(error) = &answer
        && error.is_stale_proof()
    {
        // Rebuilding needs no proofs, which only come in when the transaction is packaged.
        let unsigned = plan.build()?;
        let found = unsigned.log().txid();
        if found != txid {
            return Err(Failure::Error(Error::Rebuilt {
                expected: hex::encode(txid.0),
                found: hex::encode(found.0),
            }));
        }
        let second = offer_fresh(ctx, plan, unsigned, &offer).await?;
        warnings.extend(second.warning);
        answer = second.result;
        if let Err(error) = &answer
            && error.is_stale_proof()
        {
            return Err(Failure::Error(Error::Contested(error.clone())));
        }
    }
    match answer {
        Ok(_) => Ok(Sent::Accepted),
        Err(error @ ChainError::Transport(_)) => Ok(Sent::Unknown(error)),
        Err(error) => Err(Failure::Error(Error::Node(error))),
    }
}

/// Signs the recovery transaction, packages it with proofs fetched just now, and offers it.
async fn offer_fresh(
    ctx: &Context,
    plan: &Plan,
    unsigned: UnsignedTx,
    offer: &Offer<'_>,
) -> Result<Offered, Failure> {
    let tx = build::sign(unsigned, &[plan.authority_key]).map_err(Error::Signer)?;
    let proofs = match ctx.chain.fresh_proofs(offer.inputs).await {
        Ok(proofs) => proofs,
        Err(ChainError::NotUnspent(voucher, state)) => {
            return Err(Failure::Lost { voucher, state });
        }
        Err(error) => return Err(Failure::Error(Error::node(error))),
    };
    let bytes = build::package(tx, proofs).map_err(Error::Prover)?;
    Ok(offer::offer(ctx, offer, bytes).await)
}

/// Saves that the recovery transaction `txid` is recovering the vouchers `ids` of `allowance`.
///
/// This is idempotent, and does not undo what a reconciliation found in the meantime: a voucher
/// is marked only while it is unspent or already marked.
fn mark_pending(
    store: &mut OwnerStore,
    allowance: &str,
    ids: &[[u8; 32]],
    txid: TxID,
) -> Result<(), Error> {
    let Some(record) = store.allowance_mut(allowance) else {
        return Err(Error::Refused(format!(
            "allowance {allowance} is not in the owner's store"
        )));
    };
    for voucher in &mut record.vouchers {
        let markable = matches!(
            voucher.state,
            VoucherState::Unspent | VoucherState::RecoveryPending
        );
        if ids.contains(&voucher.id) && markable {
            voucher.state = VoucherState::RecoveryPending;
            voucher.txid = Some(txid);
        }
    }
    if !record
        .recoveries
        .iter()
        .any(|recovery| recovery.txid == txid)
    {
        record.recoveries.push(Recovery {
            txid,
            vouchers: ids.to_vec(),
            state: Progress::Pending,
        });
    }
    Ok(())
}

/// Forgets the recovery transaction `txid` of `allowance`, which the node did not take: the
/// vouchers that were waiting on it are unspent again.
fn unmark_pending(store: &mut OwnerStore, allowance: &str, txid: TxID) {
    let Some(record) = store.allowance_mut(allowance) else {
        return;
    };
    for voucher in &mut record.vouchers {
        if voucher.state == VoucherState::RecoveryPending && voucher.txid == Some(txid) {
            voucher.state = VoucherState::Unspent;
            voucher.txid = None;
        }
    }
    record
        .recoveries
        .retain(|recovery| recovery.txid != txid || recovery.state == Progress::Confirmed);
}

/// The voucher `voucher` of `plan`, which the node says is no longer unspent, as a skip.
fn lost(plan: &Plan, voucher: [u8; 32], state: &str) -> Skip {
    let qty = plan
        .vouchers
        .iter()
        .find(|(candidate, _)| candidate.id() == voucher)
        .map_or(0, |(candidate, _)| candidate.qty);
    Skip {
        id: voucher,
        qty,
        reason: format!(
            "it is no longer unspent ({state}): another transaction has taken it, probably the \
             delegate's redemption, and it is left alone"
        ),
    }
}

/// The memo of the note that pays recovered vouchers back to the wallet, which only the owner
/// reads.
const RECOVERY_MEMO: &[u8] = b"firebreak recovery";

#[cfg(test)]
mod tests {
    use curve25519_dalek::scalar::Scalar;
    use firebreak_core::devnet::LocalNode;
    use firebreak_core::journal;
    use firebreak_core::store::DelegationPackage;
    use firebreak_core::{Chain, NETWORK, keys};
    use flamekd::util;
    use flamepayments::Account;
    use tempfile::TempDir;

    use super::*;
    use crate::{CreateAllowance, Files, create_allowance, init};

    /// The delegate redeems a voucher after the owner picked it for recovery, which the owner can
    /// only learn from the node refusing to give a proof of it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_voucher_taken_after_it_was_selected_is_dropped_and_the_rest_recovered() {
        let dir = TempDir::new().expect("a temporary directory");
        let files = Files::new(dir.path().to_owned());
        let report = init(&files, 1_000).expect("init");
        let node = LocalNode::start(&report.genesis_address.to_bech32(NETWORK), 1_000).await;
        let ctx = Context::new(
            dir.path().to_owned(),
            Chain::connect(&node.url()).expect("a client"),
        );
        let delegate_key: Scalar = keys::generate(&mut OsRng);
        let merchant = Account::from_seed(&[9; 64], NETWORK, 0)
            .expect("an account")
            .address_at(util::RECEIVING, 0)
            .expect("an address");
        let request = CreateAllowance {
            merchant,
            delegate: keys::verification_key(&delegate_key),
            vouchers: vec![50, 20, 20, 10],
            wait: false,
        };
        let funded = create_allowance(&ctx, &request).await.expect("fund");
        node.mint();
        refresh(&ctx).await.expect("the vouchers are unspent");
        let ids: Vec<[u8; 32]> = funded.vouchers.iter().map(|voucher| voucher.id).collect();

        // The delegate redeems the 50 and it is confirmed, but the owner has not looked again.
        let package: DelegationPackage =
            firebreak_core::store::read(&ctx.files.package(&funded.allowance)).expect("package");
        let vouchers = package.vouchers().expect("vouchers");
        let unsigned = build::redemption(&[&vouchers[0]]).expect("build");
        let tx = build::sign(unsigned, &[delegate_key]).expect("sign");
        let proofs = ctx.chain.fresh_proofs(&ids[..1]).await.expect("proofs");
        let bytes = build::package(tx, proofs).expect("package");
        let redemption = ctx.chain.submit(bytes).await.expect("the node takes it");
        node.mint();
        assert_eq!(
            ctx.files.load().expect("store").allowances[0].vouchers[0].state,
            VoucherState::Unspent,
            "the owner's record is out of date"
        );

        let batch = Batch {
            allowance: funded.allowance.clone(),
            vouchers: ids.clone(),
        };
        let mut work = Work {
            submitted: Vec::new(),
            skipped: Vec::new(),
            warnings: Vec::new(),
        };
        recover_batch(&ctx, batch, false, &mut work)
            .await
            .expect("the others are recovered");

        // The taken voucher is left alone, with the node's reason, and the others are recovered.
        assert_eq!(work.skipped.len(), 1);
        let skip = &work.skipped[0];
        assert_eq!((skip.id, skip.qty), (ids[0], 50));
        assert!(skip.reason.contains("no longer unspent"), "{}", skip.reason);
        assert!(
            skip.reason.contains(&hex::encode(redemption.0)),
            "{}",
            skip.reason
        );
        assert_eq!(work.submitted.len(), 1);
        assert_eq!(work.submitted[0].vouchers, ids[1..]);
        assert!(work.warnings.is_empty());

        // Only the recovery that was offered is in the journal, and only it is on record.
        let entries = journal::read_all(&ctx.files.journal())
            .expect("journal")
            .entries;
        let recoveries: Vec<_> = entries
            .iter()
            .filter(|entry| entry.action == Action::Recover)
            .collect();
        assert_eq!(recoveries.len(), 1);
        assert_eq!(recoveries[0].inputs, ids[1..]);
        let store = ctx.files.load().expect("store");
        assert_eq!(store.allowances[0].recoveries.len(), 1);
        assert_eq!(
            store.allowances[0].recoveries[0].txid,
            work.submitted[0].txid
        );

        node.mint();
        refresh(&ctx).await.expect("refresh");
        let store = ctx.files.load().expect("store");
        let states: Vec<VoucherState> = store.allowances[0]
            .vouchers
            .iter()
            .map(|voucher| voucher.state)
            .collect();
        assert_eq!(
            states,
            [
                VoucherState::Redeemed,
                VoucherState::Recovered,
                VoucherState::Recovered,
                VoucherState::Recovered
            ]
        );
        drop(ctx);
        node.stop().await;
    }
}
