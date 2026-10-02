//! Paying the merchant: choosing the vouchers, redeeming them, and following the redemption.

use std::error;
use std::fmt;
use std::time::Duration;

use firebreak_core::journal::{self, Action, Actor, JournalEntry, Outcome};
use firebreak_core::select::{self, SelectError};
use firebreak_core::store::{AgentAllowance, AgentStore, VoucherState, serde_amount};
use firebreak_core::{ChainError, NETWORK, Voucher, build};
use flamekd::ReceivingAddress;
use flamevm::TxID;
use serde::Deserialize;

use crate::status::sum;
use crate::{Agent, Error, Turn};

impl Agent {
    /// Pays `request.amount` sparks to the allowance's merchant by redeeming vouchers, and
    /// waits up to `wait` for a block to confirm the redemption.
    ///
    /// A voucher pays out whole, so the amount must be exactly the sum of some of the allowance's
    /// unspent vouchers: the fewest that make it, the same ones every time. The steps are
    ///
    /// 1. the honest client's check, which is a courtesy since the chain enforces the
    ///    destination whatever this program does: the merchant must be the allowance's;
    /// 2. a reconciliation with the node, so that the vouchers' states are current;
    /// 3. the choice of vouchers, from the ones that are `unspent`;
    /// 4. the reservation: the chosen vouchers are saved as `redemption_pending`, with the id of
    ///    the transaction that will spend them, before anything is sent;
    /// 5. signing with the delegated key, then fresh membership proofs, then the submission. A
    ///    node that refuses the proofs as stale gets a rebuilt transaction once. A voucher that
    ///    turns out to be spent ends the payment, and the states are reconciled. A node that
    ///    does not answer leaves the outcome unknown: the reservation stays, and the next
    ///    reconciliation decides.
    ///
    /// Steps 2 to 5 hold the [`Turn`]. The wait for the confirmation does not, so other payments
    /// and reconciliations go on meanwhile. With no `wait` the payment is returned as soon as the
    /// node accepted it, still `redemption_pending`. Every submission is journaled.
    pub async fn pay(
        &self,
        request: &PayRequest,
        wait: Option<Duration>,
    ) -> Result<Payment, PayError> {
        let allowance = self.resolve(request).await?;
        let mut payment = self.redeem(&allowance, request.amount).await?;
        if let Some(limit) = wait {
            self.confirm(&mut payment, limit).await;
        }
        Ok(payment)
    }

    /// The id of the allowance to pay from, once the request passes the honest client's checks.
    async fn resolve(&self, request: &PayRequest) -> Result<String, PayError> {
        let records = self.files().read().await?;
        let allowance = pick(&records, request.allowance.as_deref())?;
        check_merchant(allowance, &request.merchant)?;
        Ok(allowance.allowance.clone())
    }

    /// Reserves vouchers that pay `amount`, and submits their redemption.
    async fn redeem(&self, allowance: &str, amount: u64) -> Result<Payment, PayError> {
        let turn = self.turn().await?;
        self.reconcile(&turn).await?;
        let records = self.files().read().await?;
        let vouchers = choose(&records, allowance, amount)?;
        let mut redeeming = Redeeming::new(self, &turn, allowance, vouchers);
        let txid = redeeming.submit(&records).await?;
        Ok(redeeming.into_payment(txid))
    }

    /// Waits for the redemption of `payment` to be in a block, and records the outcome.
    ///
    /// A payment that is not confirmed in time stays `redemption_pending`: that is no failure.
    async fn confirm(&self, payment: &mut Payment, limit: Duration) {
        match self.chain().wait_confirmed(&payment.txid, limit).await {
            Ok(height) => {
                payment.height = Some(height);
                if let Err(error) = self.settle(payment).await {
                    let warning = format!("the confirmed redemption is not recorded yet: {error}");
                    payment.warnings.push(warning);
                }
            }
            Err(ChainError::Timeout) => {}
            Err(error) => {
                let warning = format!("the transaction could not be followed to a block: {error}");
                payment.warnings.push(warning);
            }
        }
    }

    /// Reconciles, and marks `payment` as redeemed when every one of its vouchers is.
    async fn settle(&self, payment: &mut Payment) -> Result<(), Error> {
        {
            let turn = self.turn().await?;
            self.reconcile(&turn).await?;
        }
        let records = self.files().read().await?;
        let redeemed = payment.vouchers.iter().all(|paid| {
            records
                .allowances
                .iter()
                .flat_map(|allowance| &allowance.vouchers)
                .any(|voucher| voucher.id == paid.id && voucher.state == VoucherState::Redeemed)
        });
        if redeemed {
            payment.state = VoucherState::Redeemed;
        }
        Ok(())
    }
}

/// A request to pay a merchant out of an allowance.
///
/// In JSON, `{"allowance": "<id>", "merchant": "tf1...", "amount": "60"}`. The allowance may be
/// left out when exactly one is imported. The amount is a decimal string of sparks: a number,
/// a fraction or a negative amount is refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayRequest {
    /// The allowance to pay from.
    #[serde(default)]
    pub allowance: Option<String>,
    /// The merchant's `tf1...` address, which must be the allowance's merchant.
    pub merchant: String,
    /// The amount in sparks.
    #[serde(with = "serde_amount")]
    pub amount: u64,
}

/// A payment that the node accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Payment {
    /// The redemption transaction.
    pub txid: TxID,
    /// The vouchers it redeems.
    pub vouchers: Vec<PaidVoucher>,
    /// `redemption_pending` until a block holds the transaction, then `redeemed`.
    pub state: VoucherState,
    /// The height of the block that holds the transaction, once the agent has seen it confirmed.
    pub height: Option<u64>,
    /// Things that went wrong around the payment without stopping it, such as a journal entry
    /// that could not be written.
    pub warnings: Vec<String>,
}

/// A voucher that a payment redeems.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaidVoucher {
    /// The voucher's contract id.
    pub id: [u8; 32],
    /// What the voucher is worth, in sparks.
    pub qty: u64,
}

impl Payment {
    /// What the payment pays the merchant, in sparks.
    pub fn amount(&self) -> u64 {
        sum(self.vouchers.iter().map(|voucher| voucher.qty))
    }
}

/// Why a payment stopped, and where.
///
/// The message is the `error` and the stage is the `stage` of the JSON reply an API client gets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayError {
    /// Where the payment stopped.
    pub stage: Stage,
    /// Why, in words for the person who asked.
    pub message: String,
    /// The transaction involved, when there is one.
    pub txid: Option<TxID>,
}

impl PayError {
    /// A payment that stopped at `stage` because of `message`.
    pub fn new<M>(stage: Stage, message: M) -> PayError
    where
        M: Into<String>,
    {
        PayError {
            stage,
            message: message.into(),
            txid: None,
        }
    }
}

impl fmt::Display for PayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl error::Error for PayError {}

/// Errors of the agent's own files and node, met before anything was submitted.
impl From<Error> for PayError {
    fn from(error: Error) -> PayError {
        let stage = match &error {
            Error::Chain(_) => Stage::Node,
            Error::UnknownAllowance(_) => Stage::Input,
            _ => Stage::Store,
        };
        let message = match &error {
            Error::Chain(_) => format!("nothing was submitted: {error}"),
            _ => error.to_string(),
        };
        PayError::new(stage, message)
    }
}

impl From<SelectError> for PayError {
    fn from(error: SelectError) -> PayError {
        PayError::new(Stage::Selection, error.to_string())
    }
}

/// Where a payment stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// The request itself is not usable: it is malformed, or names no usable allowance.
    Input,
    /// The honest client's policy refused it: the merchant is not the allowance's.
    Policy,
    /// No exact subset of the unspent vouchers pays the amount.
    Selection,
    /// The agent's own files could not be read or written.
    Store,
    /// The redemption transaction could not be built.
    Prover,
    /// The redemption transaction could not be signed.
    Signer,
    /// The node could not be asked, or refused the transaction.
    Node,
    /// The node did not answer a submission, so it is unknown whether it took the transaction.
    Unknown,
}

impl Stage {
    /// The stage's name in the JSON of an error reply.
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Input => "input",
            Stage::Policy => "policy",
            Stage::Selection => "selection",
            Stage::Store => "store",
            Stage::Prover => "prover",
            Stage::Signer => "signer",
            Stage::Node => "node",
            Stage::Unknown => "unknown",
        }
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The allowance named `id`, or the only one when no id is given.
fn pick<'a>(records: &'a AgentStore, id: Option<&str>) -> Result<&'a AgentAllowance, PayError> {
    match (id, records.allowances.as_slice()) {
        (Some(id), _) => records
            .allowance(id)
            .ok_or_else(|| PayError::from(Error::UnknownAllowance(id.to_owned()))),
        (None, [only]) => Ok(only),
        (None, []) => Err(PayError::new(
            Stage::Input,
            "no allowance is imported; run `firebreak-agent import` with a delegation package",
        )),
        (None, several) => {
            let ids: Vec<&str> = several.iter().map(|a| a.allowance.as_str()).collect();
            Err(PayError::new(
                Stage::Input,
                format!(
                    "{} allowances are imported ({}); say which to pay from",
                    several.len(),
                    ids.join(", ")
                ),
            ))
        }
    }
}

/// Refuses a merchant other than the allowance's. The chain would refuse any other destination
/// anyway, because every voucher's redemption pays only its merchant.
fn check_merchant(allowance: &AgentAllowance, merchant: &str) -> Result<(), PayError> {
    let given = ReceivingAddress::from_bech32(merchant, NETWORK);
    if given.is_ok_and(|given| given == allowance.merchant) {
        return Ok(());
    }
    Err(PayError::new(
        Stage::Policy,
        format!(
            "refusing to pay {merchant}: allowance {} pays only {}, and the chain would refuse \
             any other destination anyway",
            allowance.allowance,
            allowance.merchant.to_bech32(NETWORK)
        ),
    ))
}

/// The unspent vouchers of the allowance that pay exactly `amount`, in the order they are
/// redeemed.
fn choose(records: &AgentStore, allowance: &str, amount: u64) -> Result<Vec<Voucher>, PayError> {
    let allowance = records
        .allowance(allowance)
        .ok_or_else(|| PayError::from(Error::UnknownAllowance(allowance.to_owned())))?;
    let rebuilt = allowance.vouchers().map_err(|error| {
        let message = format!(
            "the stored vouchers of allowance {} do not check out: {error}",
            allowance.allowance
        );
        PayError::new(Stage::Store, message)
    })?;
    let mut unspent: Vec<Voucher> = rebuilt
        .into_iter()
        .zip(&allowance.vouchers)
        .filter(|(_, record)| record.state == VoucherState::Unspent)
        .map(|(voucher, _)| voucher)
        .collect();
    // Vouchers of equal value are told apart by id, so the choice does not depend on the order
    // of the file.
    unspent.sort_by_key(Voucher::id);
    let available: Vec<(usize, u64)> = unspent
        .iter()
        .enumerate()
        .map(|(position, voucher)| (position, voucher.qty))
        .collect();
    let positions = select::exact_subset(&available, amount)?;
    Ok(positions
        .into_iter()
        .map(|position| unspent[position].clone())
        .collect())
}

/// A redemption on its way to the node: the vouchers it spends, and what its journal entries and
/// its caller learn about it.
struct Redeeming<'a> {
    agent: &'a Agent,
    turn: &'a Turn,
    allowance: String,
    vouchers: Vec<Voucher>,
    ids: Vec<[u8; 32]>,
    /// The id of the transaction the vouchers are reserved for, once it is known.
    txid: Option<TxID>,
    warnings: Vec<String>,
}

impl<'a> Redeeming<'a> {
    fn new(
        agent: &'a Agent,
        turn: &'a Turn,
        allowance: &str,
        vouchers: Vec<Voucher>,
    ) -> Redeeming<'a> {
        Redeeming {
            agent,
            turn,
            allowance: allowance.to_owned(),
            ids: vouchers.iter().map(Voucher::id).collect(),
            vouchers,
            txid: None,
            warnings: Vec::new(),
        }
    }

    /// Submits the redemption, and gives its transaction id once the node has accepted it.
    ///
    /// The mempool checks a membership proof against the tip as it is when the transaction
    /// arrives, so a block minted since the proofs were fetched makes the node refuse the
    /// transaction. Then the proofs are fetched again and the transaction is rebuilt, once. The
    /// bytes that were refused are never sent again.
    async fn submit(&mut self, records: &AgentStore) -> Result<TxID, PayError> {
        let mut refreshed = false;
        loop {
            let (txid, bytes) = self.package(records).await?;
            let error = match self.agent.chain().submit(bytes.clone()).await {
                Ok(_) => {
                    let node = journal::Stage::Node;
                    self.journal(node, Outcome::Accepted, Some(bytes), None)
                        .await;
                    return Ok(txid);
                }
                Err(error) => error,
            };
            let outcome = match error {
                ChainError::Transport(_) => Outcome::Unknown,
                _ => Outcome::Rejected,
            };
            let node = journal::Stage::Node;
            self.journal(node, outcome, Some(bytes), Some(error.to_string()))
                .await;
            if error.is_stale_proof() && !refreshed {
                refreshed = true;
                continue;
            }
            return Err(self.refused_by_node(error, txid).await);
        }
    }

    /// Builds the redemption transaction, signs it, and packages it with fresh proofs.
    ///
    /// Everything but the proofs depends only on the vouchers, so the transaction has the same id
    /// every time it is built. The vouchers are reserved for it before it is signed.
    async fn package(&mut self, records: &AgentStore) -> Result<(TxID, Vec<u8>), PayError> {
        let inputs: Vec<&Voucher> = self.vouchers.iter().collect();
        let unsigned = match build::redemption(&inputs) {
            Ok(unsigned) => unsigned,
            Err(error) => return Err(self.refused(Stage::Prover, error).await),
        };
        let txid = unsigned.log().txid();
        self.reserve(txid).await?;
        let signed = match build::sign(unsigned, &[records.delegate_key]) {
            Ok(signed) => signed,
            Err(error) => return Err(self.refused(Stage::Signer, error).await),
        };
        let proofs = match self.agent.chain().fresh_proofs(&self.ids).await {
            Ok(proofs) => proofs,
            Err(error) => return Err(self.unprepared(error).await),
        };
        match build::package(signed, proofs) {
            Ok(bytes) => Ok((txid, bytes)),
            Err(error) => Err(self.refused(Stage::Prover, error).await),
        }
    }

    /// Saves the vouchers as `redemption_pending` for the transaction `txid`.
    async fn reserve(&mut self, txid: TxID) -> Result<(), PayError> {
        if self.txid == Some(txid) {
            return Ok(());
        }
        let allowance = self.allowance.clone();
        let ids = self.ids.clone();
        self.agent
            .files()
            .update(move |records| {
                let found = records
                    .allowance_mut(&allowance)
                    .ok_or_else(|| Error::UnknownAllowance(allowance.clone()))?;
                for voucher in found.vouchers.iter_mut().filter(|v| ids.contains(&v.id)) {
                    voucher.state = VoucherState::RedemptionPending;
                    voucher.txid = Some(txid);
                }
                Ok(())
            })
            .await?;
        self.txid = Some(txid);
        Ok(())
    }

    /// Puts the vouchers back to `unspent` when nothing was sent for them.
    ///
    /// Only a reservation made for this redemption is released.
    async fn release(&mut self) {
        let Some(txid) = self.txid else {
            return;
        };
        let allowance = self.allowance.clone();
        let released = self
            .agent
            .files()
            .update(move |records| {
                if let Some(found) = records.allowance_mut(&allowance) {
                    for voucher in &mut found.vouchers {
                        let reserved = voucher.state == VoucherState::RedemptionPending;
                        if reserved && voucher.txid == Some(txid) {
                            voucher.state = VoucherState::Unspent;
                            voucher.txid = None;
                        }
                    }
                }
                Ok(())
            })
            .await;
        if let Err(error) = released {
            self.warnings
                .push(format!("the reservation could not be released: {error}"));
        }
    }

    /// The error for a builder or the signer refusing the transaction, which was not sent.
    async fn refused(&mut self, stage: Stage, error: firebreak_core::Error) -> PayError {
        let at = match stage {
            Stage::Signer => journal::Stage::Signer,
            _ => journal::Stage::Prover,
        };
        self.journal(at, Outcome::Rejected, None, Some(error.to_string()))
            .await;
        self.release().await;
        self.with_warnings(PayError::new(stage, error.to_string()))
    }

    /// The error for proofs that could not be fetched, which leaves the transaction unsent.
    ///
    /// A voucher that is not unspent was spent by someone else since the agent last asked, so the
    /// states are reconciled, which settles it. For any other failure the reservation is just
    /// released.
    async fn unprepared(&mut self, error: ChainError) -> PayError {
        let message = match error {
            ChainError::NotUnspent(..) => {
                // The failure being reported matters more than a reconciliation that fails too: a
                // reservation that could not be settled now is settled by the next one.
                let _ = self.agent.reconcile(self.turn).await;
                format!(
                    "{error}; the redemption was not submitted and the voucher states are updated"
                )
            }
            _ => {
                self.release().await;
                format!("{error}; the redemption was not submitted")
            }
        };
        self.with_warnings(PayError::new(Stage::Node, message))
    }

    /// The error for a submission that the node refused or did not answer.
    ///
    /// A refusal is the node's own word, so the reservation is reconciled, and the node says
    /// whether it holds the transaction after all. A submission that got no answer may have
    /// reached the node, so the reservation stays until a later reconciliation decides.
    async fn refused_by_node(&mut self, error: ChainError, txid: TxID) -> PayError {
        let mut failure = match error {
            ChainError::Transport(_) => PayError::new(
                Stage::Unknown,
                format!(
                    "{error}; transaction {} may have reached the node, so its vouchers stay \
                     redemption_pending until the node says otherwise",
                    hex::encode(txid.0)
                ),
            ),
            _ => {
                // See `unprepared` for why the result is dropped.
                let _ = self.agent.reconcile(self.turn).await;
                PayError::new(Stage::Node, error.to_string())
            }
        };
        failure.txid = Some(txid);
        self.with_warnings(failure)
    }

    /// `failure` with the warnings collected on the way added to its message.
    fn with_warnings(&self, mut failure: PayError) -> PayError {
        for warning in &self.warnings {
            failure.message.push_str(&format!(" (also: {warning})"));
        }
        failure
    }

    /// Appends an entry about this redemption to the public journal.
    ///
    /// The journal is public, so the note says how many vouchers and which allowance, and
    /// nothing about amounts.
    async fn journal(
        &mut self,
        stage: journal::Stage,
        outcome: Outcome,
        tx: Option<Vec<u8>>,
        error: Option<String>,
    ) {
        let mut entry = JournalEntry::new(Actor::Agent, Action::Redeem, stage, outcome);
        entry.txid = self.txid;
        entry.tx = tx;
        entry.inputs = self.ids.clone();
        entry.error = error;
        entry.note = Some(format!(
            "redeem {} voucher(s) of allowance {}",
            self.ids.len(),
            self.allowance
        ));
        if let Err(error) = self.agent.files().append_journal(entry).await {
            let warning = format!("the journal entry could not be written: {error}");
            self.warnings.push(warning);
        }
    }

    fn into_payment(self, txid: TxID) -> Payment {
        Payment {
            txid,
            vouchers: self
                .vouchers
                .iter()
                .map(|voucher| PaidVoucher {
                    id: voucher.id(),
                    qty: voucher.qty,
                })
                .collect(),
            state: VoucherState::RedemptionPending,
            height: None,
            warnings: self.warnings,
        }
    }
}
