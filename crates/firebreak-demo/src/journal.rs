//! The public journal: every submission attempt as its role recorded it, with each transaction
//! decoded from its own bytes.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::decode::{self, TxView};
use crate::node::TxChain;
use crate::status::Source;

/// The most entries the dashboard decodes and shows, newest first. The rest are only counted.
pub const MAX_ENTRIES: usize = 100;

/// Decoded transactions by the hex of the bytes they were decoded from, so each is verified once.
pub type Views = Mutex<HashMap<String, TxView>>;

/// The journal, newest entry first.
#[derive(Debug, Serialize)]
pub struct Journal {
    /// How many well-formed entries the file holds.
    pub total: usize,
    /// How many of them are shown: the newest.
    pub shown: usize,
    /// The lines that are not journal entries, which are ignored.
    pub malformed: Vec<Malformed>,
    pub entries: Vec<Entry>,
}

/// A journal line that could not be read.
#[derive(Debug, Serialize)]
pub struct Malformed {
    /// The line's number in the file, counting from 1.
    pub line: usize,
    pub error: String,
}

/// One submission attempt.
///
/// `actor`, `action`, `note` and `inputs` are labels the roles wrote; nothing on chain says them.
/// What the chain shows is in `tx`, which is decoded from the transaction's bytes alone.
#[derive(Debug, Serialize)]
pub struct Entry {
    /// The entry's line number in the file, counting from 1.
    pub line: usize,
    pub time: u64,
    pub actor: String,
    pub action: String,
    /// How far the attempt got: `prover`, `signer`, `local_verifier`, `node` or `confirmed`.
    pub stage: String,
    /// `accepted`, `rejected` or `unknown`.
    pub outcome: String,
    /// The actual error message of a failed attempt.
    pub error: Option<String>,
    pub note: Option<String>,
    /// The transaction ID the journal records. An attempt that never produced one has none.
    pub txid: Option<String>,
    /// The contracts the journal says the attempt spends.
    pub inputs: Vec<String>,
    /// Whether the recorded transaction ID is the one the bytes yield, when both are known.
    pub txid_matches: Option<bool>,
    /// What the node says about the transaction, added by the dashboard.
    pub chain: Option<TxChain>,
    /// The transaction decoded from its bytes, when the journal holds them.
    pub tx: Option<TxView>,
}

impl Entry {
    /// The transaction ID to ask the node about: the recorded one, else the one the verified
    /// bytes yield.
    pub fn txid_to_ask(&self) -> Option<&str> {
        let verified = self.tx.as_ref().and_then(|tx| tx.txid.as_deref());
        self.txid.as_deref().or(verified)
    }
}

/// Reads the journal at `path`, decoding the transactions of its newest entries through `views`.
pub fn read(path: &Path, views: &Views) -> Source<Journal> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Source::missing("no journal yet: nothing has been submitted");
        }
        Err(error) => return Source::failed(format!("cannot read journal.jsonl: {error}")),
    };
    let mut lines = Vec::new();
    let mut malformed = Vec::new();
    for (index, text) in text.lines().enumerate() {
        if text.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Line>(text) {
            Ok(line) => lines.push((index + 1, line)),
            Err(error) => malformed.push(Malformed {
                line: index + 1,
                error: error.to_string(),
            }),
        }
    }
    let total = lines.len();
    // Newest first: by time, and for the same second by position in the file.
    lines.sort_by(|(a_number, a), (b_number, b)| b.time.cmp(&a.time).then(b_number.cmp(a_number)));
    lines.truncate(MAX_ENTRIES);
    let transactions = describe_all(&lines, views);
    let entries: Vec<Entry> = lines
        .into_iter()
        .zip(transactions)
        .map(|((number, line), tx)| entry(number, line, tx))
        .collect();
    Source::ok(Journal {
        total,
        shown: entries.len(),
        malformed,
        entries,
    })
}

/// A journal line, as the roles write it. A line without a time, an actor, an action, a stage or
/// an outcome is not an entry; the other fields may be absent.
#[derive(Debug, Deserialize)]
struct Line {
    time: u64,
    actor: String,
    action: String,
    stage: String,
    outcome: String,
    #[serde(default)]
    txid: Option<String>,
    /// The hex of the packaged transaction, which is public.
    #[serde(default)]
    tx: Option<String>,
    #[serde(default)]
    inputs: Vec<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

/// The decoded transaction of each of `lines`, in order, from the cache where it is there.
fn describe_all(lines: &[(usize, Line)], views: &Views) -> Vec<Option<TxView>> {
    let hex_of = |line: &Line| {
        line.tx
            .as_deref()
            .filter(|hex| !hex.is_empty())
            .map(str::to_owned)
    };
    let mut cache = views.lock().unwrap_or_else(PoisonError::into_inner);
    let live: HashSet<String> = lines.iter().filter_map(|(_, line)| hex_of(line)).collect();
    cache.retain(|hex, _| live.contains(hex));
    lines
        .iter()
        .map(|(_, line)| {
            let hex = hex_of(line)?;
            let view = cache
                .entry(hex)
                .or_insert_with_key(|hex| decode::describe(hex));
            Some(view.clone())
        })
        .collect()
}

fn entry(number: usize, line: Line, tx: Option<TxView>) -> Entry {
    // The bytes' own ID is the verified one, or else the one they claim.
    let derived = tx
        .as_ref()
        .and_then(|tx| tx.txid.as_ref().or(tx.claimed_txid.as_ref()));
    let txid_matches = match (&line.txid, derived) {
        (Some(recorded), Some(derived)) => Some(recorded.eq_ignore_ascii_case(derived)),
        _ => None,
    };
    Entry {
        line: number,
        time: line.time,
        actor: line.actor,
        action: line.action,
        stage: line.stage,
        outcome: line.outcome,
        error: line.error,
        note: line.note,
        txid: line.txid,
        inputs: line.inputs,
        txid_matches,
        chain: None,
        tx,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    /// A journal line, with the transaction `tx` if there is one.
    fn line(time: u64, tx: Option<&str>) -> String {
        json!({
            "time": time, "actor": "agent", "action": "redeem", "txid": null, "tx": tx,
            "inputs": [], "stage": "node", "outcome": "accepted", "error": null, "note": null,
        })
        .to_string()
    }

    fn read_lines(lines: &[String], views: &Views) -> Journal {
        let dir = tempdir().expect("a directory");
        let path = dir.path().join("journal.jsonl");
        fs::write(&path, lines.join("\n")).expect("write the journal");
        read(&path, views).data.expect("a journal")
    }

    #[test]
    fn a_json_object_that_is_not_an_entry_is_a_bad_line() {
        let lines = [
            line(1, None),
            r#"{"hello": "world"}"#.to_owned(),
            "[1, 2]".to_owned(),
        ];
        let journal = read_lines(&lines, &Mutex::new(HashMap::new()));

        assert_eq!((journal.total, journal.shown), (1, 1));
        let bad: Vec<usize> = journal.malformed.iter().map(|bad| bad.line).collect();
        assert_eq!(bad, [2, 3]);
        assert!(
            journal.malformed[0].error.contains("missing field"),
            "{:?}",
            journal.malformed[0]
        );
    }

    #[test]
    fn entries_of_one_second_are_ordered_by_their_place_in_the_file() {
        let lines = [line(5, None), line(5, None), line(9, None), line(5, None)];
        let journal = read_lines(&lines, &Mutex::new(HashMap::new()));

        let order: Vec<(u64, usize)> = journal.entries.iter().map(|e| (e.time, e.line)).collect();
        assert_eq!(order, [(9, 3), (5, 4), (5, 2), (5, 1)]);
    }

    #[test]
    fn only_the_newest_entries_are_shown_and_the_rest_are_counted() {
        let lines: Vec<String> = (0..MAX_ENTRIES as u64 + 5)
            .map(|time| line(time, None))
            .collect();
        let journal = read_lines(&lines, &Mutex::new(HashMap::new()));

        assert_eq!(
            (journal.total, journal.shown),
            (MAX_ENTRIES + 5, MAX_ENTRIES)
        );
        assert_eq!(
            journal.entries.first().map(|entry| entry.time),
            Some(MAX_ENTRIES as u64 + 4)
        );
        assert_eq!(journal.entries.last().map(|entry| entry.time), Some(5));
    }

    #[test]
    fn a_transaction_is_decoded_once_and_forgotten_when_the_journal_drops_it() {
        let views = Mutex::new(HashMap::new());
        let both = [
            line(1, Some("00")),
            line(2, Some("ff")),
            line(3, Some("00")),
        ];
        let journal = read_lines(&both, &views);
        assert!(journal.entries.iter().all(|entry| entry.tx.is_some()));
        let mut kept: Vec<String> = views
            .lock()
            .expect("not poisoned")
            .keys()
            .cloned()
            .collect();
        kept.sort();
        assert_eq!(kept, ["00", "ff"]);

        read_lines(&[line(1, Some("00")), line(2, Some(""))], &views);
        let kept: Vec<String> = views
            .lock()
            .expect("not poisoned")
            .keys()
            .cloned()
            .collect();
        assert_eq!(kept, ["00"]);
    }
}
