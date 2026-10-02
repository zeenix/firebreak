//! Creating the merchant's wallet and publishing its address.

use std::fmt;
use std::path::PathBuf;

use firebreak_core::NETWORK;
use firebreak_core::store::{self, MerchantStore};
use flamekd::ReceivingAddress;
use rand::rngs::OsRng;

use crate::{Error, Files};

/// What `init` created.
#[derive(Debug)]
pub struct InitReport {
    /// The merchant's published address, which vouchers pay.
    pub address: ReceivingAddress,
    /// The private store that was created.
    pub store: PathBuf,
    /// The file that publishes the address.
    pub address_file: PathBuf,
}

impl fmt::Display for InitReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "created the merchant's wallet in {}\n\
             merchant address {} (published in {})",
            self.store.display(),
            self.address.to_bech32(NETWORK),
            self.address_file.display()
        )
    }
}

/// Creates the merchant's wallet and publishes its first receiving address.
///
/// This never contacts the node. It refuses to replace a wallet that exists, which would lose
/// whatever the old one was paid.
pub fn init(files: &Files) -> Result<InitReport, Error> {
    let store = MerchantStore::generate(&mut OsRng);
    let address = store.address().map_err(Error::Wallet)?;
    files.create(&store)?;
    // The address is published only once the wallet that holds it is safe on disk.
    store::write_public_line(&files.address(), &address.to_bech32(NETWORK))?;
    Ok(InitReport {
        address,
        store: files.store(),
        address_file: files.address(),
    })
}
