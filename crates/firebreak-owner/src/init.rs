//! Creating the owner's wallet, and the devnet genesis that gives it its first funds.

use std::fmt;
use std::path::PathBuf;

use curve25519_dalek::ristretto::CompressedRistretto;
use firebreak_core::store::{self, OwnerStore};
use firebreak_core::{NETWORK, keys};
use flamekd::{ReceivingAddress, util};
use rand::rngs::OsRng;

use crate::{Error, Files};

/// The amount the devnet's genesis gives the owner unless told otherwise, in sparks.
pub const DEFAULT_GENESIS_SPARKS: u64 = 1_000;

/// What `init` created.
#[derive(Debug)]
pub struct InitReport {
    /// The owner's first receiving address, which holds the genesis allocation.
    pub genesis_address: ReceivingAddress,
    /// The genesis allocation in sparks.
    pub genesis_sparks: u64,
    /// The verification key of the voucher authority, which recovers every voucher.
    pub authority: CompressedRistretto,
    /// The private store that was created.
    pub store: PathBuf,
    /// The network definition that was written.
    pub chainparams: PathBuf,
}

impl fmt::Display for InitReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "created the owner's wallet in {}\n\
             genesis address {} holds {} sparks\n\
             voucher authority (public key) {}\n\
             wrote the devnet network definition {}",
            self.store.display(),
            self.genesis_address.to_bech32(NETWORK),
            self.genesis_sparks,
            hex::encode(self.authority.to_bytes()),
            self.chainparams.display()
        )
    }
}

/// Creates the owner's wallet and voucher authority key, and writes the devnet network
/// definition that gives the wallet's first address `genesis_sparks`.
///
/// This never contacts the node, which does not have to run yet: the node starts from the network
/// definition this writes. It refuses to replace a wallet that exists.
pub fn init(files: &Files, genesis_sparks: u64) -> Result<InitReport, Error> {
    if genesis_sparks == 0 {
        return Err(Error::Refused(
            "the genesis allocation must be at least one spark".to_owned(),
        ));
    }
    let store = OwnerStore::generate(&mut OsRng);
    let genesis_address = first_address(&store).map_err(Error::Wallet)?;
    files.create(&store)?;

    let chainparams = files.chainparams();
    let definition = network_definition(&genesis_address, genesis_sparks);
    store::write_public_line(&chainparams, &definition)?;
    Ok(InitReport {
        genesis_address,
        genesis_sparks,
        authority: keys::verification_key(&store.authority_key),
        store: files.store(),
        chainparams,
    })
}

/// The wallet's first receiving address, which the genesis allocation is paid to.
fn first_address(store: &OwnerStore) -> Result<ReceivingAddress, firebreak_core::Error> {
    Ok(store.account()?.address_at(util::RECEIVING, 0)?)
}

/// The `chainparams.toml` of a devnet whose whole supply is `sparks` held by `address`.
fn network_definition(address: &ReceivingAddress, sparks: u64) -> String {
    format!(
        "version = 1\n\
         network = \"testnet\"\n\
         \n\
         # The owner's first receiving address holds the genesis allocation.\n\
         [[genesis]]\n\
         address = \"{}\"\n\
         qty_sparks = {sparks}",
        address.to_bech32(NETWORK)
    )
}
