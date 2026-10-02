//! The agent's files, and the lock that keeps concurrent processes from overwriting each other.
//!
//! Under the data directory the agent keeps
//!
//! ```text
//! agent/agent.json       the delegated key and the imported allowances (private)
//! agent/lock             guards every read-modify-write of agent.json
//! agent/turn             orders the processes that act on what the node says (see `Turn`)
//! public/delegate-key    the delegated verification key, which owners delegate to
//! public/journal.jsonl   the public journal that every role appends to
//! ```
//!
//! Several processes may use one data directory at once: the command line while the server runs.
//! `agent.json` is replaced whole and atomically, so a reader always sees one complete version.
//! A change is a read-modify-write, and it holds an exclusive lock on `agent/lock` from the read
//! to the write so that no other change falls in between. The lock is held for file operations
//! only, never while waiting for the node.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use firebreak_core::journal::{self, JournalEntry};
use firebreak_core::store::{self, AgentStore};
use rand::rngs::OsRng;

use crate::Error;

/// Where the agent's files are.
#[derive(Clone, Debug)]
pub struct Files {
    data_dir: PathBuf,
}

impl Files {
    /// The files of the agent whose data directory is `data_dir`.
    pub fn new(data_dir: PathBuf) -> Files {
        Files { data_dir }
    }

    /// The private file that holds the delegated key and the allowances.
    pub fn agent_json(&self) -> PathBuf {
        self.agent_dir().join("agent.json")
    }

    /// The public file that holds the delegated verification key as one line of hex.
    pub fn delegate_key(&self) -> PathBuf {
        self.public_dir().join("delegate-key")
    }

    /// The public journal of every role's submissions.
    pub fn journal(&self) -> PathBuf {
        self.public_dir().join("journal.jsonl")
    }

    /// Creates the agent's store with a fresh delegated key and publishes the key's verification
    /// key, which is returned as 64 hex digits.
    ///
    /// An agent that has a store keeps it: a new key would orphan every allowance that was
    /// imported for the old one.
    pub async fn init(&self) -> Result<String, Error> {
        let files = self.clone();
        blocking(move || files.init_locked()).await
    }

    /// The agent's store as it is on disk now.
    ///
    /// A change made while this reads is either seen whole or not at all, so reading needs no
    /// lock.
    pub async fn read(&self) -> Result<AgentStore, Error> {
        let path = self.agent_json();
        blocking(move || load(&path)).await
    }

    /// Applies `change` to the store as it is on disk now, and saves the result.
    ///
    /// The change runs under the lock, so it never overwrites another process's change. The
    /// store is saved only when its allowances differ afterwards, so a change that finds
    /// nothing to do writes nothing. The delegated key is never changed after
    /// [`init`](Files::init). A change that fails saves nothing.
    pub async fn update<T, F>(&self, change: F) -> Result<T, Error>
    where
        F: FnOnce(&mut AgentStore) -> Result<T, Error> + Send + 'static,
        T: Send + 'static,
    {
        let files = self.clone();
        blocking(move || files.update_locked(change)).await
    }

    /// Appends `entry` to the public journal.
    pub async fn append_journal(&self, entry: JournalEntry) -> Result<(), Error> {
        let path = self.journal();
        blocking(move || Ok(journal::append(&path, &entry)?)).await
    }

    /// Takes the exclusive lock of `agent/turn`, waiting for whoever holds it, and gives the open
    /// file. The lock lasts until the file is dropped, or until the process ends.
    pub(crate) async fn lock_turn(&self) -> Result<File, Error> {
        let files = self.clone();
        blocking(move || files.lock(TURN)).await
    }

    fn init_locked(&self) -> Result<String, Error> {
        create_private_directory(&self.agent_dir())?;
        let _lock = self.lock(LOCK)?;
        let path = self.agent_json();
        let exists = fs::exists(&path).map_err(|source| Error::io(&path, source))?;
        if exists {
            return Err(Error::AlreadyInitialised(path));
        }
        let records = AgentStore::generate(&mut OsRng);
        records.save(&path)?;
        let public = hex::encode(records.public().to_bytes());
        store::write_public_line(&self.delegate_key(), &public)?;
        Ok(public)
    }

    fn update_locked<T, F>(&self, change: F) -> Result<T, Error>
    where
        F: FnOnce(&mut AgentStore) -> Result<T, Error>,
    {
        let _lock = self.lock(LOCK)?;
        let path = self.agent_json();
        let mut records = load(&path)?;
        let before = records.allowances.clone();
        let value = change(&mut records)?;
        if records.allowances != before {
            records.save(&path)?;
        }
        Ok(value)
    }

    /// Opens the lock file `name` of the agent directory, creating it when needed, and takes an
    /// exclusive lock on it.
    fn lock(&self, name: &str) -> Result<File, Error> {
        let path = self.agent_dir().join(name);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .map_err(|source| match source.kind() {
                // Without its directory the agent was never initialised.
                io::ErrorKind::NotFound => Error::NotInitialised(self.agent_json()),
                _ => Error::io(&path, source),
            })?;
        file.lock().map_err(|source| Error::io(&path, source))?;
        Ok(file)
    }

    fn agent_dir(&self) -> PathBuf {
        self.data_dir.join("agent")
    }

    fn public_dir(&self) -> PathBuf {
        self.data_dir.join("public")
    }
}

/// Runs file work, which may wait for a lock, on a thread that is meant to block.
pub(crate) async fn blocking<T, F>(work: F) -> Result<T, Error>
where
    F: FnOnce() -> Result<T, Error> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(error) => Err(Error::Task(error.to_string())),
    }
}

/// The store saved at `path`, or [`Error::NotInitialised`] when there is none.
fn load(path: &Path) -> Result<AgentStore, Error> {
    AgentStore::load(path).map_err(|error| {
        if error.is_not_found() {
            Error::NotInitialised(path.to_path_buf())
        } else {
            Error::from(error)
        }
    })
}

/// Creates `path` and its missing parents with mode `0700`. A directory that exists keeps its
/// permissions.
fn create_private_directory(path: &Path) -> Result<(), Error> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|source| Error::io(path, source))
}

/// The lock that guards the read-modify-write of `agent.json`.
const LOCK: &str = "lock";

/// The lock that orders the processes that act on what the node says.
const TURN: &str = "turn";
