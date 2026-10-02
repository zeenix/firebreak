//! The public view of a transaction: what anyone watching the chain learns from its bytes.
//!
//! [`describe`] decodes a packaged transaction under the chain's own limits and has the VM's
//! verifier re-derive its effects, so what is shown is what the bytes do and not what a journal
//! claims. Confidential amounts stay hidden: a token appears only as the points of its Pedersen
//! commitments. Nothing here reads a quantity or a blinding factor, and nothing formats a token,
//! contract or effect with `Debug`.

use std::panic;

use firebreak_core::build::LIMITS;
use firebreak_core::voucher::{RECEIPT_KEY, TOKEN_KEY};
use flamechain::{BlockTx, ChainParams};
use flamevm::{Contract, Dict, Scalar, Token, TxEntry, TxLog, Value};
use serde::Serialize;

/// What every output's `amount` says: the observer view never shows a quantity.
pub const NOT_PUBLIC: &str = "not public";

/// What an observer learns from one transaction's bytes.
#[derive(Clone, Debug, Default, Serialize)]
pub struct TxView {
    /// Whether the bytes decode as a packaged transaction at all.
    pub decoded: bool,
    /// Why they do not.
    pub decode_error: Option<String>,
    /// The size of the packaged transaction in bytes.
    pub size_bytes: usize,
    /// The transaction ID the bytes claim, trustworthy only when `verified`.
    pub claimed_txid: Option<String>,
    /// How many Utreexo membership proofs travel with the transaction.
    pub membership_proofs: Option<usize>,
    /// Whether the VM's verifier accepted the transaction's proof, signature and effects.
    pub verified: bool,
    /// Why it did not.
    pub verification_error: Option<String>,
    /// The transaction ID re-derived from the verified effects.
    pub txid: Option<String>,
    /// The contracts the transaction spends, in order. Empty unless verified.
    pub inputs: Vec<String>,
    /// The contracts the transaction creates, in order. Empty unless verified.
    pub outputs: Vec<OutputView>,
    /// The data entries the transaction logs. Empty unless verified.
    pub data: Vec<DataView>,
    /// The fees the transaction pays, which are public. Empty unless verified.
    pub fees: Vec<FeeView>,
    /// Effects of any other kind, by name.
    pub other_effects: Vec<String>,
}

/// One contract a transaction creates.
#[derive(Clone, Debug, Serialize)]
pub struct OutputView {
    /// The contract ID.
    pub id: String,
    /// The predicate point that locks the contract.
    pub predicate: String,
    /// What the payload is.
    pub kind: &'static str,
    /// The point of the token's quantity commitment, when the payload holds a token.
    pub qty_commitment: Option<String>,
    /// The point of the token's flavor commitment, when the payload holds a token.
    pub flv_commitment: Option<String>,
    /// The length of the receipt sealed into a voucher, which is all that is shown of it.
    pub sealed_receipt_bytes: Option<usize>,
    /// What the view shows of the amount: always [`NOT_PUBLIC`], for every kind of payload.
    pub amount: &'static str,
}

/// One data entry a transaction logs.
#[derive(Clone, Debug, Serialize)]
pub struct DataView {
    /// The output this entry directly follows, as an index into the transaction's outputs.
    pub after_output: Option<usize>,
    /// The length of the entry, which is all that is shown of it.
    pub bytes: usize,
    /// What the entry is taken to be.
    pub label: &'static str,
}

/// One fee a transaction pays.
#[derive(Clone, Debug, Serialize)]
pub struct FeeView {
    /// The fee in sparks, as a decimal string. A fee is cleartext on chain.
    pub sparks: String,
}

/// Decodes the packaged transaction in `tx_hex` and verifies it, for an observer.
///
/// Every failure is part of the answer: bytes that do not decode, and a transaction that decodes
/// but does not verify, such as a rejected attack, are described honestly and without effects.
/// The bytes may be hostile, so a panic in the decoder is caught and reported the same way.
pub fn describe(tx_hex: &str) -> TxView {
    panic::catch_unwind(|| decode(tx_hex))
        .unwrap_or_else(|_| TxView::undecodable(0, "the decoder panicked on these bytes"))
}

fn decode(tx_hex: &str) -> TxView {
    let bytes = match hex::decode(tx_hex.trim()) {
        Ok(bytes) => bytes,
        Err(error) => return TxView::undecodable(0, format!("not hexadecimal: {error}")),
    };
    let params = ChainParams::default();
    let block_tx = match BlockTx::from_bytes_bounded(&bytes, params.version, params.limits) {
        Ok(block_tx) => block_tx,
        Err(error) => {
            let message = format!("not a packaged transaction: {error}");
            return TxView::undecodable(bytes.len(), message);
        }
    };
    let mut view = TxView {
        decoded: true,
        size_bytes: bytes.len(),
        claimed_txid: Some(hex::encode(block_tx.tx.txid.0)),
        membership_proofs: Some(block_tx.proofs.len()),
        ..TxView::default()
    };
    match block_tx.tx.verify(LIMITS) {
        Ok(log) => view.show_effects(&log),
        Err(error) => view.verification_error = Some(error.to_string()),
    }
    view
}

impl TxView {
    /// A view of bytes that are not a transaction.
    fn undecodable<M>(size_bytes: usize, message: M) -> TxView
    where
        M: Into<String>,
    {
        TxView {
            size_bytes,
            decode_error: Some(message.into()),
            ..TxView::default()
        }
    }

    /// Records the effects of a verified transaction, in log order.
    fn show_effects(&mut self, log: &TxLog) {
        self.verified = true;
        self.txid = Some(hex::encode(log.txid().0));
        let mut after_output = false;
        for entry in log.entries() {
            match entry {
                TxEntry::Header(_) | TxEntry::CellWitness(_) => {}
                TxEntry::Input(id) => self.inputs.push(hex::encode(id)),
                TxEntry::Output(contract) => self.outputs.push(OutputView::of(contract)),
                TxEntry::Data(bytes) => {
                    let after = after_output.then(|| self.outputs.len() - 1);
                    self.data.push(DataView::of(bytes.len(), after));
                }
                TxEntry::Fee(sparks) => self.fees.push(FeeView {
                    sparks: sparks.to_string(),
                }),
                other => self.other_effects.push(effect_name(other).to_owned()),
            }
            after_output = matches!(entry, TxEntry::Output(_));
        }
    }
}

impl OutputView {
    /// What an observer sees of `contract`: its ID, its predicate and the shape of its payload.
    fn of(contract: &Contract) -> OutputView {
        let shape = Shape::of(contract.payload());
        let (qty_commitment, flv_commitment) = match shape.commitments {
            Some((qty, flv)) => (Some(qty), Some(flv)),
            None => (None, None),
        };
        OutputView {
            id: hex::encode(contract.id()),
            predicate: hex::encode(contract.predicate.to_point().as_bytes()),
            kind: shape.kind,
            qty_commitment,
            flv_commitment,
            sealed_receipt_bytes: shape.receipt_bytes,
            amount: shape.amount,
        }
    }
}

impl DataView {
    fn of(bytes: usize, after_output: Option<usize>) -> DataView {
        DataView {
            after_output,
            bytes,
            label: if after_output.is_some() {
                NOTE_LABEL
            } else {
                DATA_LABEL
            },
        }
    }
}

/// The label of a data entry that directly follows an output.
const NOTE_LABEL: &str = "encrypted receipt/note";

/// The label of any other data entry.
const DATA_LABEL: &str = "data entry";

/// What a contract's payload is, as far as an observer may say.
struct Shape {
    kind: &'static str,
    /// The points of the token's quantity and flavor commitments.
    commitments: Option<(String, String)>,
    receipt_bytes: Option<usize>,
    amount: &'static str,
}

impl Shape {
    fn of(payload: &Value) -> Shape {
        match payload {
            Value::Token(token) => Shape {
                kind: "confidential token",
                commitments: Some(commitment_points(token)),
                receipt_bytes: None,
                amount: NOT_PUBLIC,
            },
            Value::ClearToken(_) => Shape::other("clear token", NOT_PUBLIC),
            Value::Dict(dict) => Shape::voucher(dict)
                .unwrap_or_else(|| Shape::other("dictionary payload", NOT_PUBLIC)),
            _ => Shape::other("other payload", NOT_PUBLIC),
        }
    }

    /// A payload that shows no commitments.
    fn other(kind: &'static str, amount: &'static str) -> Shape {
        Shape {
            kind,
            commitments: None,
            receipt_bytes: None,
            amount,
        }
    }

    /// A voucher: a dictionary of exactly a token and the receipt sealed for the merchant.
    fn voucher(dict: &Dict) -> Option<Shape> {
        if dict.len() != 2 {
            return None;
        }
        // `get_resolved` reports a pruned branch as an error, where `get` would panic.
        let mut dict = dict.clone();
        let commitments = match dict.get_resolved(&Scalar::from(TOKEN_KEY), &mut ()) {
            Ok(Some(Value::Token(token))) => commitment_points(token),
            _ => return None,
        };
        let receipt_bytes = match dict.get_resolved(&Scalar::from(RECEIPT_KEY), &mut ()) {
            Ok(Some(Value::String(receipt))) => receipt.len(),
            _ => return None,
        };
        Some(Shape {
            kind: "voucher (token + sealed receipt)",
            commitments: Some(commitments),
            receipt_bytes: Some(receipt_bytes),
            amount: NOT_PUBLIC,
        })
    }
}

/// The points of a token's two commitments: the only thing about it an observer may see.
fn commitment_points(token: &Token) -> (String, String) {
    (
        hex::encode(token.qty().to_point().as_bytes()),
        hex::encode(token.flv().to_point().as_bytes()),
    )
}

/// The name of an effect.
fn effect_name(entry: &TxEntry) -> &'static str {
    match entry {
        TxEntry::Header(_) => "header",
        TxEntry::CellWitness(_) => "witness",
        TxEntry::Data(_) => "data",
        TxEntry::Input(_) => "input",
        TxEntry::Receive(_) => "receive",
        TxEntry::ActorDeploy { .. } => "actor deploy",
        TxEntry::Output(_) => "output",
        TxEntry::IssuePub(..) => "cleartext issuance",
        TxEntry::IssuePriv(..) => "confidential issuance",
        TxEntry::Retire(..) => "retirement",
        TxEntry::Fee(_) => "fee",
        TxEntry::ActorSave { .. } => "actor save",
        TxEntry::SetCode { .. } => "actor code replacement",
        TxEntry::Send(_) => "message",
        TxEntry::StoragePurchase { .. } => "storage purchase",
        TxEntry::ActorDestroy { .. } => "actor destruction",
    }
}

#[cfg(test)]
mod tests {
    use firebreak_core::devnet::{Allowance, Devnet, Parties, fund, output, rng};
    use firebreak_core::voucher::Voucher;
    use firebreak_core::{Payout, WalletInput, build};
    use flamekd::util;
    use flamepayments::{Opening, prepare_output};
    use serde_json::Value as Json;

    use super::*;

    /// A funded allowance on a devnet, and the parties to spend it.
    struct Funded {
        parties: Parties,
        devnet: Devnet,
        allowance: Allowance,
    }

    fn funded() -> Funded {
        let mut rng = rng(7);
        let parties = Parties::new(&mut rng);
        let mut devnet = Devnet::new(&parties.owner);
        let allowance = fund(&mut devnet, &parties, &mut rng);
        Funded {
            parties,
            devnet,
            allowance,
        }
    }

    /// The hex of the transaction that redeems `paying`, signed by the delegate.
    fn redemption(funded: &Funded, paying: &[&Voucher]) -> String {
        let unsigned = build::redemption(paying).expect("build the redemption");
        let tx = build::sign(unsigned, &[funded.parties.delegate_key]).expect("sign");
        package(funded, tx, paying)
    }

    /// The hex of `tx`, packaged with the current proofs of the vouchers it spends.
    fn package(funded: &Funded, tx: flamevm::ExternalTx, spending: &[&Voucher]) -> String {
        let proofs = spending
            .iter()
            .map(|voucher| funded.devnet.proof(&voucher.id()))
            .collect();
        hex::encode(build::package(tx, proofs).expect("package the transaction"))
    }

    /// The points of the commitments of the token a voucher holds.
    fn token_points(voucher: &Voucher) -> (String, String) {
        let Value::Dict(dict) = voucher.contract.payload() else {
            panic!("a voucher holds a dictionary");
        };
        let Some(Value::Token(token)) = dict.get(&Scalar::from(TOKEN_KEY)) else {
            panic!("a voucher holds a token");
        };
        commitment_points(token)
    }

    /// Checks that nothing a page would receive for `view` reveals a quantity or an opening.
    fn assert_hides(view: &TxView, openings: &[Opening], amounts: &[u64]) {
        let mut hidden: Vec<String> = amounts.iter().map(u64::to_string).collect();
        let mut encoded = Vec::new();
        for opening in openings {
            encoded.push(hex::encode(Scalar::from(opening.qty).to_bytes()));
            encoded.push(hex::encode(opening.qty_blinding.to_bytes()));
            encoded.push(hex::encode(opening.flv_blinding.to_bytes()));
        }
        hidden.extend(encoded.iter().cloned());
        let json = serde_json::to_value(view).expect("serialize the view");
        assert_no_secrets(&json, &hidden);
        // Neither as a whole field nor inside a longer one.
        let text = json.to_string();
        for secret in &encoded {
            assert!(!text.contains(secret.as_str()), "an opening leaked");
        }
    }

    /// Checks that no string of `json` is one of `hidden`, and no field is named for a secret.
    fn assert_no_secrets(json: &Json, hidden: &[String]) {
        match json {
            Json::String(text) => assert!(!hidden.contains(text), "{text} is a secret"),
            Json::Array(items) => items
                .iter()
                .for_each(|item| assert_no_secrets(item, hidden)),
            Json::Object(fields) => {
                for (name, value) in fields {
                    let secret_names = ["qty", "quantity", "blinding", "opening", "secret"];
                    assert!(
                        !secret_names.contains(&name.as_str()),
                        "a field named {name}"
                    );
                    assert_no_secrets(value, hidden);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn a_redemption_shows_the_vouchers_it_spends_and_hides_every_amount() {
        let funded = funded();
        let vouchers = &funded.allowance.vouchers;
        let paying = [&vouchers[0], &vouchers[3]];
        let view = describe(&redemption(&funded, &paying));

        assert!(view.decoded, "{:?}", view.decode_error);
        assert!(view.verified, "{:?}", view.verification_error);
        assert_eq!(view.membership_proofs, Some(2));
        assert_eq!(view.txid, view.claimed_txid);
        let spent: Vec<String> = paying.iter().map(|v| hex::encode(v.id())).collect();
        assert_eq!(view.inputs, spent);

        // Each voucher became a token under the merchant's key with the same commitments.
        let merchant = funded.parties.merchant_address().spending_key().compress();
        assert_eq!(view.outputs.len(), 2);
        for (output, voucher) in view.outputs.iter().zip(paying) {
            let (qty, flv) = token_points(voucher);
            assert_eq!(output.predicate, hex::encode(merchant.as_bytes()));
            assert_eq!(output.kind, "confidential token");
            assert_eq!(output.qty_commitment, Some(qty));
            assert_eq!(output.flv_commitment, Some(flv));
            assert_eq!(output.sealed_receipt_bytes, None);
            assert_eq!(output.amount, "not public");
        }

        // Each payout is followed by the voucher's receipt, of which only the length shows.
        assert_eq!(view.data.len(), 2);
        for (index, (data, voucher)) in view.data.iter().zip(paying).enumerate() {
            assert_eq!(data.after_output, Some(index));
            assert_eq!(data.bytes, voucher.receipt().expect("a receipt").len());
            assert_eq!(data.label, "encrypted receipt/note");
        }
        assert!(view.fees.is_empty() && view.other_effects.is_empty());

        assert_hides(
            &view,
            &funded.allowance.openings,
            &[10, 20, 50, 60, 100, 900, 1000],
        );
    }

    #[test]
    fn a_funding_transaction_shows_its_vouchers_as_commitments_and_sealed_receipts() {
        let mut rng = rng(9);
        let parties = Parties::new(&mut rng);
        let devnet = Devnet::new(&parties.owner);
        let policy = parties.policy(&mut rng);
        let merchant = parties.merchant_address();
        let voucher = prepare_output(&output(merchant, 50), &mut rng).expect("prepare a voucher");
        let to = parties
            .owner
            .address_at(util::CHANGE, 0)
            .expect("a change address");
        let change = prepare_output(&output(to, 950), &mut rng).expect("prepare the change");
        let payouts = [
            Payout::Voucher {
                policy: &policy,
                prepared: &voucher,
            },
            Payout::Wallet {
                to: to.spending_key().compress(),
                prepared: &change,
            },
        ];
        let key = parties
            .owner
            .spending_key_at(util::RECEIVING, 0)
            .expect("the owner's key");
        let proof = devnet.proof(&devnet.genesis.id());
        let input = WalletInput::clear(devnet.genesis.clone(), proof, key).expect("a clear input");
        let unsigned = build::funding(std::slice::from_ref(&input), &payouts, 0).expect("build");
        let tx = build::sign(unsigned, &[input.signing_key()]).expect("sign the funding");
        let bytes = build::package(tx, vec![input.proof().clone()]).expect("package");
        let view = describe(&hex::encode(bytes));

        assert!(view.verified, "{:?}", view.verification_error);
        assert_eq!(view.inputs, [hex::encode(devnet.genesis.id())]);
        let [vouchered, changed] = &view.outputs[..] else {
            panic!("a voucher and the change");
        };
        let points = |prepared: &flamepayments::PreparedOutput| {
            let (qty, flv) = (prepared.token.qty(), prepared.token.flv());
            (
                hex::encode(qty.to_point().as_bytes()),
                hex::encode(flv.to_point().as_bytes()),
            )
        };
        assert_eq!(vouchered.kind, "voucher (token + sealed receipt)");
        let locked = policy.predicate().expect("a predicate");
        assert_eq!(vouchered.predicate, hex::encode(locked.as_bytes()));
        let (qty, flv) = points(&voucher);
        assert_eq!(vouchered.qty_commitment, Some(qty));
        assert_eq!(vouchered.flv_commitment, Some(flv));
        assert_eq!(vouchered.sealed_receipt_bytes, Some(voucher.note.len()));
        assert_eq!(vouchered.amount, "not public");

        let change_key = to.spending_key().compress();
        assert_eq!(changed.kind, "confidential token");
        assert_eq!(changed.predicate, hex::encode(change_key.as_bytes()));
        let (qty, flv) = points(&change);
        assert_eq!(changed.qty_commitment, Some(qty));
        assert_eq!(changed.flv_commitment, Some(flv));
        assert_eq!(changed.amount, "not public");

        // The change's note is the one data entry, right after the change output.
        let [note] = &view.data[..] else {
            panic!("one note");
        };
        assert_eq!(note.after_output, Some(1));
        assert_eq!(note.bytes, change.note.len());

        assert_hides(&view, &[voucher.opening, change.opening], &[50, 950, 1000]);
    }

    #[test]
    fn a_transaction_signed_with_the_wrong_key_decodes_but_does_not_verify() {
        let funded = funded();
        let voucher = &funded.allowance.vouchers[1];
        // The owner's key does not authorize the delegate's branch: an attacker's failed attempt.
        let unsigned = build::redemption(&[voucher]).expect("build the redemption");
        let tx = build::sign_positionally(unsigned, &[funded.parties.owner_key]).expect("sign");
        let view = describe(&package(&funded, tx, &[voucher]));

        assert!(view.decoded);
        assert!(!view.verified);
        let error = view.verification_error.expect("the verifier says why");
        assert!(!error.is_empty());
        assert!(
            view.claimed_txid.is_some(),
            "the claimed ID is shown, as a claim"
        );
        assert!(
            view.txid.is_none(),
            "nothing is re-derived from an unverified transaction"
        );
        assert!(view.inputs.is_empty() && view.outputs.is_empty() && view.data.is_empty());
    }

    #[test]
    fn a_truncated_transaction_does_not_decode() {
        let funded = funded();
        let voucher = &funded.allowance.vouchers[1];
        let whole = hex::decode(redemption(&funded, &[voucher])).expect("hex");
        let half = &whole[..whole.len() / 2];
        let view = describe(&hex::encode(half));

        assert!(!view.decoded && !view.verified);
        let error = view.decode_error.expect("the decoder says why");
        assert!(error.starts_with("not a packaged transaction"), "{error}");
        assert_eq!(view.size_bytes, half.len());
    }

    #[test]
    fn text_that_is_not_hex_does_not_decode() {
        let view = describe("not hex at all");
        assert!(!view.decoded);
        assert!(
            view.decode_error
                .expect("a reason")
                .starts_with("not hexadecimal")
        );

        let view = describe("");
        assert!(!view.decoded);
        assert!(
            view.decode_error
                .expect("a reason")
                .starts_with("not a packaged")
        );
    }
}
