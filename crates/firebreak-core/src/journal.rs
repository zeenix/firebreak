//! The public journal: one JSON object per line, appended by every role for every submission
//! attempt.
//!
//! The journal says what was tried and how far it got. The submitted bytes are public, so an
//! observer can decode each attempt and check it again; the stage and the error say where it
//! stopped and why. A journal line never holds a secret.
//!
//! In `journal.jsonl`, each line is
//!
//! ```json
//! {"time": 1696000000, "actor": "owner", "action": "fund", "txid": "<hex or null>",
//!  "tx": "<hex BlockTx bytes or null>", "inputs": ["<contract id hex>"], "stage": "confirmed",
//!  "outcome": "accepted", "error": null, "note": "<short description or null>"}
//! ```
//!
//! where `actor` is `owner`, `agent`, `merchant` or `attacker`; `action` is `fund`, `redeem`,
//! `recover`, `spend` or `attack:<name>`; `stage` is the last stage the attempt reached, one of
//! `prover`, `signer`, `local_verifier`, `node` and `confirmed`; and `outcome` is `accepted`,
//! `rejected` or `unknown`.

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::str::FromStr;

use flamevm::TxID;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::store::{self, StoreError, serde_hex};

/// One submission attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    /// When the attempt happened, in Unix seconds.
    pub time: u64,
    /// Who made the attempt.
    pub actor: Actor,
    /// What the attempt tried to do.
    pub action: Action,
    /// The transaction's id, when the attempt got far enough to have one.
    #[serde(default, with = "serde_hex::option")]
    pub txid: Option<TxID>,
    /// The bytes submitted to the node, when the attempt got far enough to have them.
    #[serde(default, with = "serde_hex::option")]
    pub tx: Option<Vec<u8>>,
    /// The contracts the transaction spends.
    #[serde(with = "serde_hex::vec")]
    pub inputs: Vec<[u8; 32]>,
    /// The last stage the attempt reached.
    pub stage: Stage,
    /// How the attempt ended, as far as its maker knows.
    pub outcome: Outcome,
    /// The error that stopped it, in the words of the stage that raised it.
    #[serde(default)]
    pub error: Option<String>,
    /// A short description of the attempt for a human reader.
    #[serde(default)]
    pub note: Option<String>,
}

impl JournalEntry {
    /// An entry stamped with the current time, with no transaction, inputs, error or note yet.
    pub fn new(actor: Actor, action: Action, stage: Stage, outcome: Outcome) -> JournalEntry {
        JournalEntry {
            time: store::unix_now(),
            actor,
            action,
            txid: None,
            tx: None,
            inputs: Vec::new(),
            stage,
            outcome,
            error: None,
            note: None,
        }
    }
}

/// Appends `entry` to the journal at `path` as one line, creating the file and its directories
/// when needed.
///
/// The line is written in a single call to a file opened for appending, so the lines that several
/// processes append do not interleave.
pub fn append(path: &Path, entry: &JournalEntry) -> Result<(), StoreError> {
    let line = serde_json::to_vec(entry).map_err(|source| StoreError::json(path, source))?;
    store::append_line(path, &line)
}

/// Reads every entry of the journal at `path`, in the order they were appended.
///
/// A line that is not a journal entry is skipped and counted in [`Journal::skipped`], which is
/// what a line cut short by a crash looks like. Blank lines are ignored and not counted. A
/// journal that does not exist yet is an empty one.
pub fn read_all(path: &Path) -> Result<Journal, StoreError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Journal::default()),
        Err(source) => return Err(StoreError::io(path, source)),
    };
    let mut journal = Journal::default();
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<JournalEntry>(line) {
            Ok(entry) => journal.entries.push(entry),
            Err(_) => journal.skipped += 1,
        }
    }
    Ok(journal)
}

/// The entries of a journal file, and how many of its lines were not entries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Journal {
    /// The entries, oldest first.
    pub entries: Vec<JournalEntry>,
    /// How many non-blank lines were skipped because they are not journal entries.
    pub skipped: usize,
}

/// Who made an attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Actor {
    /// The owner, funding or recovering an allowance.
    Owner,
    /// The delegate, paying a merchant.
    Agent,
    /// The merchant, spending what it received.
    Merchant,
    /// Someone trying what an honest role never would.
    Attacker,
}

/// What an attempt tried to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Funding an allowance.
    Fund,
    /// Redeeming vouchers to their merchant.
    Redeem,
    /// Recovering vouchers to the owner.
    Recover,
    /// The merchant spending what it received.
    Spend,
    /// A named attack, written `attack:<name>`.
    Attack(String),
}

impl fmt::Display for Action {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Action::Fund => formatter.write_str("fund"),
            Action::Redeem => formatter.write_str("redeem"),
            Action::Recover => formatter.write_str("recover"),
            Action::Spend => formatter.write_str("spend"),
            Action::Attack(name) => write!(formatter, "attack:{name}"),
        }
    }
}

impl FromStr for Action {
    type Err = UnknownAction;

    fn from_str(text: &str) -> Result<Action, UnknownAction> {
        match text {
            "fund" => Ok(Action::Fund),
            "redeem" => Ok(Action::Redeem),
            "recover" => Ok(Action::Recover),
            "spend" => Ok(Action::Spend),
            _ => match text.strip_prefix("attack:") {
                Some(name) if !name.is_empty() => Ok(Action::Attack(name.to_owned())),
                _ => Err(UnknownAction(text.to_owned())),
            },
        }
    }
}

impl Serialize for Action {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Action {
    fn deserialize<D>(deserializer: D) -> Result<Action, D::Error>
    where
        D: Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(de::Error::custom)
    }
}

/// The text of an action that is none of the journal's.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown action {0:?}; expected fund, redeem, recover, spend or attack:<name>")]
pub struct UnknownAction(String);

/// The last stage an attempt reached, in the order an honest transaction passes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// The prover refused to build the transaction.
    Prover,
    /// The signer refused to sign it.
    Signer,
    /// The local verifier rejected the finished transaction.
    LocalVerifier,
    /// The node received it.
    Node,
    /// A block confirmed it.
    Confirmed,
}

/// How an attempt ended, as far as its maker knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// The stage accepted the transaction.
    Accepted,
    /// The stage refused it.
    Rejected,
    /// The maker could not learn what became of it, for instance after a lost connection.
    Unknown,
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::Write;

    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    fn fund() -> JournalEntry {
        JournalEntry {
            time: 1_696_000_000,
            actor: Actor::Owner,
            action: Action::Fund,
            txid: Some(TxID([1; 32])),
            tx: Some(vec![0xde, 0xad, 0xbe, 0xef]),
            inputs: vec![[2; 32], [3; 32]],
            stage: Stage::Confirmed,
            outcome: Outcome::Accepted,
            error: None,
            note: Some("funded an allowance of 100".to_owned()),
        }
    }

    fn rejected() -> JournalEntry {
        let mut entry = JournalEntry::new(
            Actor::Attacker,
            Action::Attack("forged_leaf".to_owned()),
            Stage::Node,
            Outcome::Rejected,
        );
        entry.error = Some("Merkle proof is invalid\nsecond line".to_owned());
        entry
    }

    fn journal_in(dir: &TempDir) -> std::path::PathBuf {
        dir.path().join("public/journal.jsonl")
    }

    #[test]
    fn an_entry_has_exactly_the_json_of_the_spec() {
        let json = serde_json::to_value(fund()).expect("serialize");
        assert_eq!(
            json,
            json!({
                "time": 1_696_000_000,
                "actor": "owner",
                "action": "fund",
                "txid": "01".repeat(32),
                "tx": "deadbeef",
                "inputs": ["02".repeat(32), "03".repeat(32)],
                "stage": "confirmed",
                "outcome": "accepted",
                "error": null,
                "note": "funded an allowance of 100"
            })
        );

        let mut early = rejected();
        early.stage = Stage::LocalVerifier;
        let json = serde_json::to_value(early).expect("serialize");
        assert_eq!(json["action"], "attack:forged_leaf");
        assert_eq!(json["actor"], "attacker");
        assert_eq!(json["stage"], "local_verifier");
        assert_eq!(json["outcome"], "rejected");
        assert!(json["txid"].is_null() && json["tx"].is_null() && json["note"].is_null());
        assert_eq!(json["inputs"], json!([]));
    }

    #[test]
    fn every_actor_stage_and_outcome_of_the_spec_is_written_as_the_spec_names_it() {
        let names = |values: Vec<serde_json::Value>| -> Vec<String> {
            values
                .iter()
                .map(|value| value.as_str().expect("text").to_owned())
                .collect()
        };
        let actors = [Actor::Owner, Actor::Agent, Actor::Merchant, Actor::Attacker];
        assert_eq!(
            names(actors.iter().map(|actor| json!(actor)).collect()),
            ["owner", "agent", "merchant", "attacker"]
        );
        let stages = [
            Stage::Prover,
            Stage::Signer,
            Stage::LocalVerifier,
            Stage::Node,
            Stage::Confirmed,
        ];
        assert_eq!(
            names(stages.iter().map(|stage| json!(stage)).collect()),
            ["prover", "signer", "local_verifier", "node", "confirmed"]
        );
        let outcomes = [Outcome::Accepted, Outcome::Rejected, Outcome::Unknown];
        assert_eq!(
            names(outcomes.iter().map(|outcome| json!(outcome)).collect()),
            ["accepted", "rejected", "unknown"]
        );
    }

    #[test]
    fn actions_are_written_and_read_as_text() {
        for (action, text) in [
            (Action::Fund, "fund"),
            (Action::Redeem, "redeem"),
            (Action::Recover, "recover"),
            (Action::Spend, "spend"),
            (Action::Attack("key_path".to_owned()), "attack:key_path"),
        ] {
            assert_eq!(action.to_string(), text);
            assert_eq!(text.parse::<Action>(), Ok(action));
        }
        for text in ["", "attack", "attack:", "Fund", "pay", "attack key_path"] {
            assert_eq!(
                text.parse::<Action>(),
                Err(UnknownAction(text.to_owned())),
                "{text:?}"
            );
        }
    }

    #[test]
    fn appended_entries_come_back_in_order() {
        let dir = TempDir::new().expect("a temporary directory");
        let path = journal_in(&dir);
        let second = rejected();
        for entry in [fund(), second.clone(), fund()] {
            append(&path, &entry).expect("append");
        }
        let journal = read_all(&path).expect("read");
        assert_eq!(journal.skipped, 0);
        assert_eq!(journal.entries, [fund(), second, fund()]);
    }

    #[test]
    fn every_entry_is_one_line_even_when_its_text_has_newlines() {
        let dir = TempDir::new().expect("a temporary directory");
        let path = journal_in(&dir);
        append(&path, &rejected()).expect("append");
        append(&path, &fund()).expect("append");
        let text = fs::read_to_string(&path).expect("read");
        assert_eq!(text.lines().count(), 2);
        assert!(text.ends_with('\n'));
        assert!(
            text.lines().next().expect("a line").contains("\\n"),
            "the newline is escaped"
        );
        assert_eq!(
            read_all(&path).expect("read").entries[0].error.as_deref(),
            Some("Merkle proof is invalid\nsecond line")
        );
    }

    #[test]
    fn appending_creates_the_file_and_its_directories() {
        let dir = TempDir::new().expect("a temporary directory");
        let path = dir.path().join("a/b/c/journal.jsonl");
        assert!(!path.exists());
        append(&path, &fund()).expect("append");
        assert!(path.is_file());
    }

    #[test]
    fn lines_that_are_not_entries_are_skipped_and_counted() {
        let dir = TempDir::new().expect("a temporary directory");
        let path = journal_in(&dir);
        append(&path, &fund()).expect("append");

        let mut file = OpenOptions::new().append(true).open(&path).expect("open");
        let first_half = serde_json::to_string(&fund()).expect("serialize");
        let valid_but_wrong_shape = json!({"time": 1}).to_string();
        for line in [
            "not json at all".to_owned(),
            valid_but_wrong_shape,
            // An action the spec does not name, and a stage it does not name.
            serde_json::to_string(&fund())
                .expect("serialize")
                .replace("\"fund\"", "\"pay\""),
            serde_json::to_string(&fund())
                .expect("serialize")
                .replace("confirmed", "mempool"),
            // A line cut short by a crash.
            first_half[..first_half.len() / 2].to_owned(),
        ] {
            writeln!(file, "{line}").expect("write");
        }
        // Bytes that are not even text.
        file.write_all(&[0xff, 0xfe, b'\n']).expect("write");
        drop(file);
        append(&path, &fund()).expect("append");

        let journal = read_all(&path).expect("read");
        assert_eq!(journal.entries, [fund(), fund()]);
        assert_eq!(journal.skipped, 6);
    }

    #[test]
    fn blank_lines_are_ignored_and_not_counted() {
        let dir = TempDir::new().expect("a temporary directory");
        let path = journal_in(&dir);
        append(&path, &fund()).expect("append");
        let mut file = OpenOptions::new().append(true).open(&path).expect("open");
        file.write_all(b"\n   \n\r\n").expect("write");
        drop(file);
        append(&path, &fund()).expect("append");

        let journal = read_all(&path).expect("read");
        assert_eq!(journal.entries.len(), 2);
        assert_eq!(journal.skipped, 0);
    }

    #[test]
    fn a_missing_journal_is_an_empty_one() {
        let dir = TempDir::new().expect("a temporary directory");
        let journal = read_all(&dir.path().join("absent.jsonl")).expect("read");
        assert_eq!(journal, Journal::default());
    }

    #[test]
    fn a_path_that_is_a_directory_is_an_error_not_an_empty_journal() {
        let dir = TempDir::new().expect("a temporary directory");
        let error = read_all(dir.path()).expect_err("a directory is not a journal");
        assert!(matches!(error, StoreError::Io { .. }), "{error}");
        assert!(!error.is_not_found());
    }

    #[test]
    fn entries_written_by_another_process_with_extra_fields_still_read() {
        let mut json = serde_json::to_value(fund()).expect("serialize");
        json["extra"] = json!("ignored");
        let dir = TempDir::new().expect("a temporary directory");
        let path = journal_in(&dir);
        fs::create_dir_all(path.parent().expect("a parent")).expect("create");
        fs::write(&path, format!("{json}\n")).expect("write");
        assert_eq!(read_all(&path).expect("read").entries, [fund()]);
    }

    #[test]
    fn a_new_entry_is_stamped_with_the_current_time() {
        let before = store::unix_now();
        let entry = JournalEntry::new(
            Actor::Agent,
            Action::Redeem,
            Stage::Prover,
            Outcome::Rejected,
        );
        assert!(entry.time >= before && entry.time <= store::unix_now());
        assert_eq!(entry.txid, None);
        assert_eq!(entry.tx, None);
        assert!(entry.inputs.is_empty());
        assert_eq!(entry.error, None);
        assert_eq!(entry.note, None);
    }
}
