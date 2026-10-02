//! Creating the owner's wallet.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use firebreak_core::store::OwnerStore;
use firebreak_core::{NETWORK, keys};
use firebreak_owner::{DEFAULT_GENESIS_SPARKS, Error, Files, init};
use flamed::config::ChainParamsFile;
use flamekd::util;
use tempfile::TempDir;

fn mode(path: &std::path::Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

#[test]
fn init_creates_a_private_wallet_and_a_genesis_the_node_accepts() {
    let dir = TempDir::new().expect("a temporary directory");
    let files = Files::new(dir.path().join("data"));
    let report = init(&files, 1_000).expect("init");

    // The wallet is private: the store and the lock are for the owner alone, in a directory too.
    assert_eq!(mode(&files.store()), 0o600);
    assert_eq!(mode(&files.lock()), 0o600);
    assert_eq!(mode(files.store().parent().expect("a directory")), 0o700);

    // The report names the wallet's first address and the authority's public key.
    let store = OwnerStore::load(&files.store()).expect("the store");
    let first = store
        .account()
        .expect("an account")
        .address_at(util::RECEIVING, 0)
        .expect("an address");
    assert_eq!(report.genesis_address, first);
    assert_eq!(
        report.authority,
        keys::verification_key(&store.authority_key)
    );
    assert_eq!(store.allowances.len(), 0);
    let shown = report.to_string();
    assert!(shown.contains(&first.to_bech32(NETWORK)), "{shown}");
    assert!(
        !shown.contains(&hex::encode(store.wallet_seed))
            && !shown.contains(&hex::encode(store.authority_key.to_bytes())),
        "the report names no secret"
    );

    // The network definition is exactly what the node reads, and gives the wallet the genesis.
    let params = ChainParamsFile::load(&files.chainparams()).expect("chainparams.toml");
    assert_eq!(params.version, 1);
    assert_eq!(params.genesis.len(), 1);
    assert_eq!(params.genesis[0].qty_sparks, 1_000);
    assert_eq!(
        params.genesis[0].address.as_deref(),
        Some(first.to_bech32(NETWORK).as_str())
    );
    let genesis = dir.path().join("genesis.json");
    flamed::genesis::write(&params, &genesis).expect("the node derives a genesis from it");
}

#[test]
fn init_does_not_replace_a_wallet() {
    let dir = TempDir::new().expect("a temporary directory");
    let files = Files::new(dir.path().to_owned());
    init(&files, DEFAULT_GENESIS_SPARKS).expect("init");
    let before = fs::read(files.store()).expect("the store");
    let params = fs::read(files.chainparams()).expect("chainparams.toml");

    let error = init(&files, 5).expect_err("a second init");
    assert!(matches!(error, Error::Refused(_)), "{error}");
    assert!(error.to_string().contains("already exists"), "{error}");
    assert_eq!(fs::read(files.store()).expect("the store"), before);
    assert_eq!(
        fs::read(files.chainparams()).expect("chainparams.toml"),
        params
    );
}

#[test]
fn init_refuses_a_genesis_of_nothing() {
    let dir = TempDir::new().expect("a temporary directory");
    let files = Files::new(dir.path().to_owned());
    let error = init(&files, 0).expect_err("no sparks");
    assert!(matches!(error, Error::Refused(_)), "{error}");
    assert!(!files.store().exists(), "no wallet was made");
}

#[test]
fn a_command_before_init_says_to_run_init() {
    let dir = TempDir::new().expect("a temporary directory");
    let files = Files::new(dir.path().to_owned());
    let error = files.load().expect_err("no wallet");
    assert!(matches!(error, Error::Refused(_)), "{error}");
    assert!(
        error.to_string().contains("firebreak-owner init"),
        "{error}"
    );
}
