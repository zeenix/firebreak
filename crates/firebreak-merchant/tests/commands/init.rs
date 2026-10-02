//! Creating the merchant's wallet and publishing its address.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use firebreak_core::NETWORK;
use firebreak_core::store::MerchantStore;
use firebreak_merchant::{Error, Files, init};
use tempfile::TempDir;

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

#[test]
fn init_creates_a_private_wallet_and_publishes_its_address() {
    let dir = TempDir::new().expect("a temporary directory");
    let files = Files::new(dir.path().join("data"));
    let report = init(&files).expect("init");

    // The wallet is private: the store and the lock are for the merchant alone, in a directory
    // that is too.
    assert_eq!(mode(&files.store()), 0o600);
    assert_eq!(mode(&files.lock()), 0o600);
    assert_eq!(mode(files.store().parent().expect("a directory")), 0o700);

    // The published address is one line, and is the wallet's first receiving address.
    let store = MerchantStore::load(&files.store()).expect("the store");
    let address = store.address().expect("an address");
    assert_eq!(report.address, address);
    let published = fs::read_to_string(files.address()).expect("the published address");
    assert_eq!(published, format!("{}\n", address.to_bech32(NETWORK)));
    assert!(published.starts_with("tf1"));
    assert_eq!(
        files.address(),
        dir.path().join("data/public/merchant-address")
    );
    assert_eq!(store.next_index, 1);
    assert!(store.receipts.is_empty() && store.spends.is_empty());
    let shown = report.to_string();
    assert!(shown.contains(&address.to_bech32(NETWORK)), "{shown}");
    assert!(
        !shown.contains(&hex::encode(store.seed)),
        "the report names no secret"
    );
}

#[test]
fn init_does_not_replace_a_wallet() {
    let dir = TempDir::new().expect("a temporary directory");
    let files = Files::new(dir.path().to_owned());
    init(&files).expect("init");
    let store = fs::read(files.store()).expect("the store");
    let address = fs::read(files.address()).expect("the address");

    let error = init(&files).expect_err("a second init");
    assert!(matches!(error, Error::Refused(_)), "{error}");
    assert!(error.to_string().contains("already exists"), "{error}");
    assert_eq!(fs::read(files.store()).expect("the store"), store);
    assert_eq!(
        fs::read(files.address()).expect("the address"),
        address,
        "the published address is still the wallet's"
    );
}

#[test]
fn a_command_before_init_says_to_run_init() {
    let dir = TempDir::new().expect("a temporary directory");
    let files = Files::new(dir.path().to_owned());
    let error = files.load().expect_err("no wallet");
    assert!(matches!(error, Error::Refused(_)), "{error}");
    assert!(
        error.to_string().contains("firebreak-merchant init"),
        "{error}"
    );
}
