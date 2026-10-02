//! Importing the delegation packages that owners write.

use std::path::{Path, PathBuf};

use firebreak_core::store::{self, AgentStore, DelegationPackage, Import};
use flamekd::ReceivingAddress;

use crate::status::sum;
use crate::{Error, Files};

/// Imports the delegation packages in the files `paths`, in order.
///
/// Every voucher of a package is checked against its policy, and the package must delegate to
/// this agent's key. Importing a package again changes nothing. Either every package is
/// imported or, when one is refused, none is: the error names the file.
pub async fn packages(files: &Files, paths: Vec<PathBuf>) -> Result<Vec<Imported>, Error> {
    files
        .update(move |records| paths.iter().map(|path| import_one(records, path)).collect())
        .await
}

/// What importing one delegation package did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Imported {
    /// The allowance's id.
    pub allowance: String,
    /// The merchant the allowance pays.
    pub merchant: ReceivingAddress,
    /// How many vouchers the package lists.
    pub vouchers: usize,
    /// What the vouchers are worth together, in sparks.
    pub total: u64,
    /// Whether the allowance is new to the agent.
    pub outcome: Import,
}

fn import_one(records: &mut AgentStore, path: &Path) -> Result<Imported, Error> {
    let package: DelegationPackage = store::read(path)?;
    let outcome = records.import(&package).map_err(|source| Error::Package {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Imported {
        allowance: package.allowance.clone(),
        merchant: package.merchant,
        vouchers: package.vouchers.len(),
        total: sum(package.vouchers.iter().map(|voucher| voucher.qty)),
        outcome,
    })
}
