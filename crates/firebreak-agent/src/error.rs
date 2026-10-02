//! Why an operation of the agent failed.

use std::io;
use std::path::{Path, PathBuf};

use firebreak_core::ChainError;
use firebreak_core::store::StoreError;

/// Why an operation of the agent failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A file of the agent could not be read, written or understood.
    #[error("{0}")]
    Store(#[from] StoreError),

    /// A directory or a lock file could not be opened.
    #[error("{}: {source}", .path.display())]
    Io {
        /// The directory or file involved.
        path: PathBuf,
        /// What happened.
        source: io::Error,
    },

    /// The agent has no store yet.
    #[error("{} does not exist; run `firebreak-agent init` first", .0.display())]
    NotInitialised(PathBuf),

    /// The agent has a store already, and its key must not be replaced.
    #[error("{} exists already; the agent keeps the key it has", .0.display())]
    AlreadyInitialised(PathBuf),

    /// A delegation package was refused.
    #[error("{}: {source}", .path.display())]
    Package {
        /// The package file.
        path: PathBuf,
        /// Why it was refused.
        source: firebreak_core::Error,
    },

    /// No allowance with this id was imported.
    #[error("no allowance {0} is imported")]
    UnknownAllowance(String),

    /// The node could not be asked.
    #[error("{0}")]
    Chain(#[from] ChainError),

    /// A background task panicked or was cancelled.
    #[error("a background task failed: {0}")]
    Task(String),
}

impl Error {
    /// A failure of the file system at `path`.
    pub(crate) fn io(path: &Path, source: io::Error) -> Error {
        Error::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}
