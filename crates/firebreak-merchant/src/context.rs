//! Where the merchant's files are, and the node the commands talk to.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use firebreak_core::Chain;
use firebreak_core::store::{MerchantStore, StoreError};

use crate::Error;

/// How long `--wait` waits for a transaction to be confirmed.
pub const CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(60);

/// Everything a command that talks to the node needs.
pub struct Context {
    /// The merchant's files.
    pub files: Files,
    /// The node.
    pub chain: Chain,
    /// How long a command that waits for a confirmation waits, [`CONFIRMATION_TIMEOUT`] unless
    /// changed.
    pub confirmation_timeout: Duration,
}

impl Context {
    /// A context for the files under `data_dir` and the node `chain` talks to.
    pub fn new(data_dir: PathBuf, chain: Chain) -> Context {
        Context {
            files: Files::new(data_dir),
            chain,
            confirmation_timeout: CONFIRMATION_TIMEOUT,
        }
    }
}

/// The merchant's files under one data directory.
///
/// The private store is `merchant/merchant.json`, and every change to it goes through
/// [`Files::update`], which holds the lock file `merchant/lock` for the whole of the change. What
/// the delegate, the dashboard and the public read lives in `public/`.
#[derive(Clone, Debug)]
pub struct Files {
    data: PathBuf,
}

impl Files {
    /// The files under the data directory `data`.
    pub fn new(data: PathBuf) -> Files {
        Files { data }
    }

    /// The private store, which holds the wallet seed and what the merchant has received.
    pub fn store(&self) -> PathBuf {
        self.data.join("merchant").join("merchant.json")
    }

    /// The lock file that guards the private store.
    pub fn lock(&self) -> PathBuf {
        self.data.join("merchant").join("lock")
    }

    /// The file that publishes the merchant's address.
    pub fn address(&self) -> PathBuf {
        self.public().join("merchant-address")
    }

    /// The snapshot of the merchant's state that the dashboard reads.
    pub fn status(&self) -> PathBuf {
        self.public().join("merchant-status.json")
    }

    /// The public journal of every submission attempt.
    pub fn journal(&self) -> PathBuf {
        self.public().join("journal.jsonl")
    }

    /// The store as it is saved now.
    ///
    /// Use [`Files::update`] to change it: a store that is read here and written back later
    /// overwrites whatever changed in between.
    pub fn load(&self) -> Result<MerchantStore, Error> {
        match MerchantStore::load(&self.store()) {
            Ok(store) => Ok(store),
            Err(error) if error.is_not_found() => Err(Error::Refused(format!(
                "there is no merchant wallet at {}; run `firebreak-merchant init` first",
                self.store().display()
            ))),
            Err(error) => Err(error.into()),
        }
    }

    /// Lets `change` edit the saved store, and saves what it leaves.
    ///
    /// The store is read, changed and written back under the exclusive lock, so changes made by
    /// other processes are never lost. Nothing is saved when `change` fails. The lock is held
    /// only while `change` runs, which cannot wait for the node: it is not asynchronous.
    pub fn update<T, F>(&self, change: F) -> Result<T, Error>
    where
        F: FnOnce(&mut MerchantStore) -> Result<T, Error>,
    {
        let _lock = Lock::acquire(&self.lock())?;
        let mut store = self.load()?;
        let value = change(&mut store)?;
        store.save(&self.store())?;
        Ok(value)
    }

    /// Saves `store` as the merchant's first store, refusing to replace one that exists.
    pub fn create(&self, store: &MerchantStore) -> Result<(), Error> {
        let _lock = Lock::acquire(&self.lock())?;
        let path = self.store();
        let exists = path
            .try_exists()
            .map_err(|source| io_error(&path, source))?;
        if exists {
            return Err(Error::Refused(format!(
                "{} already exists; refusing to overwrite the merchant's wallet",
                path.display()
            )));
        }
        store.save(&path)?;
        Ok(())
    }

    /// The directory of the files other roles and the dashboard read.
    fn public(&self) -> PathBuf {
        self.data.join("public")
    }
}

/// The exclusive lock on a role's private store, held until it is dropped.
///
/// The lock is an advisory lock on a file of its own, so it works while the store itself is
/// replaced by renaming a new file over it.
struct Lock {
    file: File,
}

impl Lock {
    /// Waits for the lock file at `path` to be free and locks it, creating the file, and the
    /// private directory it lives in, when they are missing.
    fn acquire(path: &Path) -> Result<Lock, StoreError> {
        if let Some(directory) = path.parent() {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(directory)
                .map_err(|source| io_error(directory, source))?;
        }
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .map_err(|source| io_error(path, source))?;
        file.lock().map_err(|source| io_error(path, source))?;
        Ok(Lock { file })
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Closing the file releases the lock anyway; failing to say so earlier changes nothing.
        let _ = self.file.unlock();
    }
}

/// The error for a file or directory that the file system refused.
fn io_error(path: &Path, source: io::Error) -> StoreError {
    StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}
