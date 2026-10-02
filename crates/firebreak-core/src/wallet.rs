//! A wallet's outputs, found on the chain.
//!
//! A Flame wallet owns whatever sits under the predicate of one of its addresses. [`sync`] asks the
//! node for everything under the predicates of a range of addresses and works out what each
//! output is worth and how to spend it. A confidential token reveals nothing on the chain, so its
//! value and openings come from the note that followed it, opened with the address's viewing key.
//! The genesis allocation is the one output whose token is in the clear.

use std::collections::HashMap;
use std::fmt;
use std::ops::Range;

use curve25519_dalek::scalar::Scalar;
use flamechain::utreexo::Proof;
use flamekd::{ReceivingAddress, util};
use flamepayments::{Account, NoteError, Opening, open_note};
use flamevm::{Contract, FLAME_FLAVOR, TxID, Value};

use crate::chain::{Chain, ScanHit};
use crate::{Error, WalletInput};

/// Finds the outputs of `account`'s addresses `receiving` on the receiving branch and `change` on
/// the change branch, spent and unspent.
///
/// An output under one of those addresses that the wallet cannot use, such as a token whose note
/// does not open, is left out of [`Synced::outputs`] and listed in [`Synced::skipped`], so that a
/// payment someone else crafted can never make the sync fail.
pub async fn sync(
    chain: &Chain,
    account: &Account,
    receiving: Range<u32>,
    change: Range<u32>,
) -> Result<Synced, Error> {
    let mut addresses = HashMap::new();
    for (branch, indices) in [(util::RECEIVING, receiving), (util::CHANGE, change)] {
        for index in indices {
            let address = account.address_at(branch, index)?;
            let predicate = address.spending_key().compress().to_bytes();
            addresses.insert(predicate, (branch, index, address));
        }
    }

    let predicates: Vec<[u8; 32]> = addresses.keys().copied().collect();
    let mut synced = Synced::default();
    for hit in chain.scan(&predicates, 0).await? {
        let predicate = hit.contract.predicate.to_point().to_bytes();
        // The chain answers only for the predicates asked about; anything else is not ours.
        let Some(&(branch, index, address)) = addresses.get(&predicate) else {
            continue;
        };
        let view_key = account.viewing_key_at(branch, index)?;
        match claim(&hit, branch, index, &address, &view_key) {
            Ok(output) => synced.outputs.push(output),
            Err(reason) => synced.skipped.push(Skipped {
                id: hit.id,
                branch,
                index,
                reason,
            }),
        }
    }
    Ok(synced)
}

/// What a sync found.
#[derive(Debug, Default)]
pub struct Synced {
    /// The wallet's outputs, spent and unspent, oldest first.
    pub outputs: Vec<OwnedOutput>,
    /// The outputs under the wallet's addresses that it cannot use.
    pub skipped: Vec<Skipped>,
}

/// An output of a wallet.
#[derive(Clone)]
pub struct OwnedOutput {
    /// The contract's id.
    pub id: [u8; 32],
    /// The contract as published.
    pub contract: Contract,
    /// What the output is worth, in sparks.
    pub qty: u64,
    /// What spends the output when its token is confidential. The cleartext genesis allocation
    /// has none.
    pub opening: Option<Opening>,
    /// The wallet branch of the address that holds the output: receiving or change.
    pub branch: u32,
    /// The index of that address.
    pub index: u32,
    /// The height of the block that created the output. Genesis allocations are at height 0.
    pub height: u64,
    /// The transaction that created the output.
    pub txid: TxID,
    /// The memo of the output's note, empty for a cleartext output.
    pub memo: Vec<u8>,
    /// The height of the block that spent the output and the transaction that did, or `None`
    /// while it is unspent.
    pub spent: Option<(u64, TxID)>,
}

impl OwnedOutput {
    /// The output as an input of a transaction, with the key of its address and a membership
    /// `proof` that is valid right now.
    ///
    /// A cleartext token is spent as published and a confidential one with its opening, which is
    /// refused unless it rebuilds the published contract.
    pub fn to_input(&self, account: &Account, proof: Proof) -> Result<WalletInput, Error> {
        let key = account.spending_key_at(self.branch, self.index)?;
        match &self.opening {
            None => WalletInput::clear(self.contract.clone(), proof, key),
            Some(opening) => WalletInput::confidential(&self.contract, opening, proof, key),
        }
    }
}

impl fmt::Debug for OwnedOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OwnedOutput")
            .field("id", &hex::encode(self.id))
            .field("branch", &self.branch)
            .field("index", &self.index)
            .field("height", &self.height)
            .field("spent", &self.spent.is_some())
            .finish_non_exhaustive()
    }
}

/// An output under one of a wallet's addresses that the wallet cannot use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Skipped {
    /// The output's contract id.
    pub id: [u8; 32],
    /// The wallet branch of the address that holds it.
    pub branch: u32,
    /// The index of that address.
    pub index: u32,
    /// Why the wallet cannot use it.
    pub reason: SkipReason,
}

impl fmt::Display for Skipped {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "output {} at address {}/{} was left out: {}",
            hex::encode(self.id),
            self.branch,
            self.index,
            self.reason
        )
    }
}

/// Why a wallet cannot use an output under its address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SkipReason {
    /// The contract holds something other than a token.
    #[error("it holds something other than a token")]
    NotAToken,

    /// The token is not of the native flavor.
    #[error("its token is not of the native flavor")]
    ForeignFlavor,

    /// The token is in the clear and its amount does not fit in 64 bits.
    #[error("its cleartext amount does not fit in 64 bits")]
    Amount,

    /// The token is confidential and its note does not open.
    #[error("its note does not open: {0}")]
    Note(NoteError),
}

/// The output a scan hit under `address` stands for, or why the wallet cannot use it.
fn claim(
    hit: &ScanHit,
    branch: u32,
    index: u32,
    address: &ReceivingAddress,
    view_key: &Scalar,
) -> Result<OwnedOutput, SkipReason> {
    let (qty, opening, memo) = match hit.contract.payload() {
        Value::ClearToken(token) => {
            if token.flv() != FLAME_FLAVOR {
                return Err(SkipReason::ForeignFlavor);
            }
            let qty = token.qty().to_u64().ok_or(SkipReason::Amount)?;
            (qty, None, Vec::new())
        }
        Value::Token(_) => {
            let note = open_note(&hit.contract, hit.note.as_deref(), address, view_key)
                .map_err(SkipReason::Note)?;
            if note.opening.flv != FLAME_FLAVOR {
                return Err(SkipReason::ForeignFlavor);
            }
            (note.opening.qty, Some(note.opening), note.memo)
        }
        _ => return Err(SkipReason::NotAToken),
    };
    Ok(OwnedOutput {
        id: hit.id,
        contract: hit.contract.clone(),
        qty,
        opening,
        branch,
        index,
        height: hit.height,
        txid: hit.txid,
        memo,
        spent: hit.spent,
    })
}

#[cfg(test)]
mod tests {
    use flamepayments::{OutputSpec, prepare_output};
    use flamevm::{Anchor, ClearToken, Predicate};
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    use super::*;
    use crate::{ChainError, NETWORK};

    fn account(seed: u8) -> Account {
        Account::from_seed(&[seed; 64], NETWORK, 0).expect("an account")
    }

    /// A scan hit for `contract`, as the node would report it.
    fn hit(contract: Contract, note: Option<Vec<u8>>) -> ScanHit {
        ScanHit {
            id: contract.id(),
            contract,
            note,
            height: 4,
            txid: TxID([7; 32]),
            spent: None,
        }
    }

    /// A contract under `address` holding `payload`.
    fn contract_under(address: &ReceivingAddress, payload: Value) -> Contract {
        Contract::new(
            Predicate::opaque(address.spending_key().compress()),
            Anchor([3; 32]),
            payload,
        )
        .expect("a portable payload")
    }

    /// The confidential token of `qty` and its note, paid to `address`.
    fn token_for(address: ReceivingAddress, qty: u64, flv: flamevm::Scalar) -> (Contract, Vec<u8>) {
        let spec = OutputSpec {
            address,
            qty,
            flv,
            memo: b"hello".to_vec(),
        };
        let prepared = prepare_output(&spec, &mut StdRng::seed_from_u64(qty)).expect("prepare");
        (
            contract_under(&address, Value::Token(prepared.token)),
            prepared.note,
        )
    }

    fn claim_at(account: &Account, hit: &ScanHit) -> Result<OwnedOutput, SkipReason> {
        let address = account.address_at(util::CHANGE, 2).expect("an address");
        let view_key = account.viewing_key_at(util::CHANGE, 2).expect("a view key");
        claim(hit, util::CHANGE, 2, &address, &view_key)
    }

    #[test]
    fn a_cleartext_token_is_owned_at_its_published_amount_with_no_opening() {
        let account = account(1);
        let address = account.address_at(util::CHANGE, 2).expect("an address");
        let clear = ClearToken::new(flamevm::Scalar::from(1_000u64), FLAME_FLAVOR);
        let contract = contract_under(&address, Value::ClearToken(clear));
        let output = claim_at(&account, &hit(contract, None)).expect("owned");

        assert_eq!(output.qty, 1_000);
        assert!(output.opening.is_none());
        assert!(output.memo.is_empty());
        assert_eq!(
            (output.branch, output.index, output.height),
            (util::CHANGE, 2, 4)
        );
        assert_eq!(output.txid, TxID([7; 32]));
        assert_eq!(output.spent, None);
    }

    #[test]
    fn a_confidential_token_is_opened_from_its_note() {
        let account = account(1);
        let address = account.address_at(util::CHANGE, 2).expect("an address");
        let (contract, note) = token_for(address, 900, FLAME_FLAVOR);
        let mut found = hit(contract, Some(note));
        found.spent = Some((6, TxID([8; 32])));
        let output = claim_at(&account, &found).expect("owned");

        assert_eq!(output.qty, 900);
        assert_eq!(output.memo, b"hello");
        assert_eq!(output.opening.expect("an opening").qty, 900);
        assert_eq!(output.spent, Some((6, TxID([8; 32]))));
    }

    #[test]
    fn a_token_whose_note_does_not_open_is_skipped_for_the_reason_the_note_gives() {
        let account = account(1);
        let address = account.address_at(util::CHANGE, 2).expect("an address");
        let (contract, note) = token_for(address, 900, FLAME_FLAVOR);

        let missing = claim_at(&account, &hit(contract.clone(), None));
        assert_eq!(missing.err(), Some(SkipReason::Note(NoteError::Missing)));

        let mut corrupt = note.clone();
        *corrupt.last_mut().expect("a note") ^= 1;
        let corrupt = claim_at(&account, &hit(contract.clone(), Some(corrupt)));
        assert_eq!(
            corrupt.err(),
            Some(SkipReason::Note(NoteError::Undecryptable))
        );

        // A note sealed to someone else's address does not decrypt under this one.
        let other = self::account(2)
            .address_at(util::CHANGE, 2)
            .expect("an address");
        let (_, elsewhere) = token_for(other, 900, FLAME_FLAVOR);
        let wrong = claim_at(&account, &hit(contract, Some(elsewhere)));
        assert!(matches!(wrong, Err(SkipReason::Note(_))));
    }

    #[test]
    fn a_token_of_another_flavor_is_skipped() {
        let account = account(1);
        let address = account.address_at(util::CHANGE, 2).expect("an address");
        let other_flavor = flamevm::Scalar::from(77u64);

        let clear = ClearToken::new(flamevm::Scalar::from(5u64), other_flavor);
        let contract = contract_under(&address, Value::ClearToken(clear));
        let skipped = claim_at(&account, &hit(contract, None));
        assert_eq!(skipped.err(), Some(SkipReason::ForeignFlavor));

        let (contract, note) = token_for(address, 5, other_flavor);
        let skipped = claim_at(&account, &hit(contract, Some(note)));
        assert_eq!(skipped.err(), Some(SkipReason::ForeignFlavor));
    }

    #[test]
    fn a_cleartext_amount_beyond_sixty_four_bits_is_skipped() {
        let account = account(1);
        let address = account.address_at(util::CHANGE, 2).expect("an address");
        let huge = ClearToken::new(flamevm::Scalar::from(u128::MAX), FLAME_FLAVOR);
        let contract = contract_under(&address, Value::ClearToken(huge));
        let skipped = claim_at(&account, &hit(contract, None));
        assert_eq!(skipped.err(), Some(SkipReason::Amount));
    }

    #[test]
    fn a_contract_that_is_not_a_token_is_skipped() {
        let account = account(1);
        let address = account.address_at(util::CHANGE, 2).expect("an address");
        let payload = Value::String(flamevm::String::from(b"not money".to_vec()));
        let contract = contract_under(&address, payload);
        let skipped = claim_at(&account, &hit(contract, None));
        assert_eq!(skipped.err(), Some(SkipReason::NotAToken));
    }

    #[test]
    fn an_owned_output_becomes_an_input_with_the_key_of_its_address() {
        let account = account(1);
        let address = account.address_at(util::CHANGE, 2).expect("an address");
        let key = account.spending_key_at(util::CHANGE, 2).expect("a key");

        let clear = ClearToken::new(flamevm::Scalar::from(1_000u64), FLAME_FLAVOR);
        let contract = contract_under(&address, Value::ClearToken(clear));
        let output = claim_at(&account, &hit(contract, None)).expect("owned");
        let input = output
            .to_input(&account, Proof::Transient)
            .expect("a clear input");
        assert_eq!(input.signing_key(), key);

        let (contract, note) = token_for(address, 900, FLAME_FLAVOR);
        let output = claim_at(&account, &hit(contract, Some(note))).expect("owned");
        let input = output
            .to_input(&account, Proof::Transient)
            .expect("a confidential input");
        assert_eq!(input.signing_key(), key);
    }

    #[test]
    fn an_opening_that_does_not_open_the_contract_is_not_an_input() {
        let account = account(1);
        let address = account.address_at(util::CHANGE, 2).expect("an address");
        let (contract, note) = token_for(address, 900, FLAME_FLAVOR);
        let mut output = claim_at(&account, &hit(contract, Some(note))).expect("owned");
        let (other_contract, other_note) = token_for(address, 20, FLAME_FLAVOR);
        let other = claim_at(&account, &hit(other_contract, Some(other_note))).expect("owned");
        output.opening = other.opening;

        let Err(error) = output.to_input(&account, Proof::Transient) else {
            panic!("the opening is another token's");
        };
        assert!(matches!(error, Error::OpeningMismatch), "{error}");
    }

    #[test]
    fn a_token_without_its_opening_is_not_an_input() {
        // A confidential contract spent as if it were in the clear is refused.
        let account = account(1);
        let address = account.address_at(util::CHANGE, 2).expect("an address");
        let (contract, note) = token_for(address, 900, FLAME_FLAVOR);
        let mut output = claim_at(&account, &hit(contract, Some(note))).expect("owned");
        output.opening = None;
        let Err(error) = output.to_input(&account, Proof::Transient) else {
            panic!("a token is not a cleartext input");
        };
        assert!(matches!(error, Error::NotAWalletOutput), "{error}");
    }

    #[test]
    fn debug_output_names_no_amount_and_no_opening() {
        let account = account(1);
        let address = account.address_at(util::CHANGE, 2).expect("an address");
        let (contract, note) = token_for(address, 123_456, FLAME_FLAVOR);
        let output = claim_at(&account, &hit(contract, Some(note))).expect("owned");
        let shown = format!("{output:?}");
        assert!(shown.contains("OwnedOutput"));
        assert!(!shown.contains("123456") && !shown.contains("qty") && !shown.contains("opening"));
        assert!(!shown.contains("hello"), "the memo is not shown");
    }

    /// Whether `future` can run on a multi-threaded runtime.
    fn is_send<F>(_: &F)
    where
        F: Future + Send,
    {
    }

    #[test]
    fn a_sync_can_run_on_a_multi_threaded_runtime() {
        let chain = Chain::connect("http://127.0.0.1:1").expect("a client");
        let account = account(1);
        is_send(&sync(&chain, &account, 0..1, 0..1));
    }

    #[tokio::test]
    async fn a_sync_with_no_node_fails_with_the_nodes_error() {
        // Nothing listens on port 1, so the scan cannot be answered.
        let chain = Chain::connect("http://127.0.0.1:1").expect("a client");
        let error = sync(&chain, &account(1), 0..1, 0..1)
            .await
            .expect_err("no node");
        assert!(
            matches!(error, Error::Chain(ChainError::Transport(_))),
            "{error}"
        );
    }

    #[tokio::test]
    async fn syncing_no_addresses_asks_nobody_and_finds_nothing() {
        let chain = Chain::connect("http://127.0.0.1:1").expect("a client");
        let synced = sync(&chain, &account(1), 0..0, 0..0)
            .await
            .expect("nothing to ask");
        assert!(synced.outputs.is_empty() && synced.skipped.is_empty());
    }

    #[test]
    fn a_skipped_output_explains_itself() {
        let skipped = Skipped {
            id: [0xab; 32],
            branch: util::CHANGE,
            index: 3,
            reason: SkipReason::Note(NoteError::Undecryptable),
        };
        let shown = skipped.to_string();
        assert!(shown.contains(&"ab".repeat(32)));
        assert!(shown.contains("1/3"));
        assert!(shown.contains("does not decrypt"), "{shown}");
    }
}
