//! The agent: its files, its node, and the turn that orders everything that acts on the node's
//! answers.

use std::fs::File;
use std::sync::Arc;

use firebreak_core::Chain;
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::{Error, Files};

/// A process's handle on the agent: its files and its node.
///
/// Share it between tasks with an [`Arc`]. Every operation reloads the agent's files from disk,
/// so the handle holds no copy of them.
#[derive(Debug)]
pub struct Agent {
    files: Files,
    chain: Chain,
    queue: Arc<Mutex<()>>,
}

impl Agent {
    /// The agent whose files are `files`, talking to the node through `chain`.
    pub fn new(files: Files, chain: Chain) -> Agent {
        Agent {
            files,
            chain,
            queue: Arc::new(Mutex::new(())),
        }
    }

    /// The agent's files.
    pub fn files(&self) -> &Files {
        &self.files
    }

    /// The agent's node.
    pub fn chain(&self) -> &Chain {
        &self.chain
    }

    /// Waits for the turn and takes it.
    ///
    /// Take it once, at the start of an operation, and hand the [`Turn`] to whatever needs
    /// proof of it: taking it again while holding it waits for oneself forever.
    pub async fn turn(&self) -> Result<Turn, Error> {
        let queue = Arc::clone(&self.queue).lock_owned().await;
        let file = self.files.lock_turn().await?;
        Ok(Turn {
            _file: file,
            _queue: queue,
        })
    }
}

/// The right to change what the agent believes about its vouchers from what the node says.
///
/// A voucher the agent has reserved for a payment is `redemption_pending` while its transaction
/// is on its way to the node, and the node does not know the transaction yet. Reconciling in
/// that moment would take the reservation for a transaction that was dropped and release it.
/// So the payment and every reconciliation each take the turn, and only one of them has it at a
/// time. A mutex orders the tasks of one process, and an exclusive lock on `agent/turn` orders
/// the processes that share a data directory. The operating system drops the lock when its
/// holder ends, so a payment that crashed halfway never keeps anyone out.
///
/// A payment does not hold the turn while it waits for a confirmation, only while it reserves
/// vouchers and submits their transaction. The turn is always taken before the lock on
/// `agent.json`, never after it.
#[derive(Debug)]
pub struct Turn {
    _file: File,
    _queue: OwnedMutexGuard<()>,
}
