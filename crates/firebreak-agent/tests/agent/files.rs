//! The agent's files: initialising, importing, and changing the store from several processes.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use firebreak_agent::{Error, Files, import};
use firebreak_core::store::{self, Import, VoucherState};
use flamevm::TxID;
use tempfile::tempdir;

use crate::fixture::paper_package;

fn mode(path: &std::path::Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

#[tokio::test]
async fn init_makes_a_private_store_and_publishes_only_the_verification_key() {
    let dir = tempdir().expect("a directory");
    let files = Files::new(dir.path().to_path_buf());
    let public = files.init().await.expect("init");

    assert_eq!(public.len(), 64);
    let published = fs::read_to_string(files.delegate_key()).expect("the public key");
    assert_eq!(published, format!("{public}\n"));
    assert_eq!(mode(&dir.path().join("agent")), 0o700);
    assert_eq!(mode(&files.agent_json()), 0o600);

    // The secret key is in the store and nowhere in public.
    let records = files.read().await.expect("the store");
    assert_eq!(hex::encode(records.public().to_bytes()), public);
    let secret = hex::encode(records.delegate_key.to_bytes());
    assert!(!published.contains(&secret));
    assert!(!public.contains(&secret));

    // An agent that has a store keeps its key.
    let error = files.init().await.expect_err("a second init");
    assert!(matches!(error, Error::AlreadyInitialised(_)), "{error}");
    let again = files.read().await.expect("the store");
    assert_eq!(again.delegate_key, records.delegate_key);
}

#[tokio::test]
async fn an_agent_that_was_never_initialised_says_so() {
    let dir = tempdir().expect("a directory");
    let files = Files::new(dir.path().to_path_buf());
    let error = files.read().await.expect_err("nothing to read");
    assert!(matches!(error, Error::NotInitialised(_)), "{error}");
    assert!(
        error.to_string().contains("firebreak-agent init"),
        "{error}"
    );
    let error = files
        .update(|_| Ok(()))
        .await
        .expect_err("nothing to change");
    assert!(matches!(error, Error::NotInitialised(_)), "{error}");
}

#[tokio::test]
async fn importing_twice_changes_nothing_and_a_package_for_another_agent_is_refused() {
    let dir = tempdir().expect("a directory");
    let files = Files::new(dir.path().to_path_buf());
    files.init().await.expect("init");
    let records = files.read().await.expect("the store");
    let package = paper_package(&records, &[50, 20, 20, 10], 1);
    let path = dir.path().join("public").join("allowance-one.json");
    store::write_public(&path, &package).expect("write the package");

    let first = import::packages(&files, vec![path.clone()])
        .await
        .expect("import");
    let [imported] = first.as_slice() else {
        panic!("expected one package");
    };
    assert_eq!(imported.outcome, Import::Added);
    assert_eq!(
        (
            imported.allowance.as_str(),
            imported.vouchers,
            imported.total
        ),
        (package.allowance.as_str(), 4, 100)
    );
    let saved = files.read().await.expect("the store");
    assert_eq!(saved.allowances.len(), 1);
    assert!(
        saved.allowances[0]
            .vouchers
            .iter()
            .all(|v| v.state == VoucherState::Unknown)
    );

    let again = import::packages(&files, vec![path.clone()])
        .await
        .expect("import again");
    assert_eq!(again[0].outcome, Import::AlreadyImported);
    assert_eq!(
        files.read().await.expect("the store").allowances,
        saved.allowances
    );

    // A package for the key of another agent is refused, naming its file.
    let stranger_dir = tempdir().expect("a directory");
    let stranger = Files::new(stranger_dir.path().to_path_buf());
    stranger.init().await.expect("init");
    let foreign = paper_package(&stranger.read().await.expect("the store"), &[5], 2);
    let foreign_path = dir.path().join("public").join("allowance-foreign.json");
    store::write_public(&foreign_path, &foreign).expect("write the package");
    let error = import::packages(&files, vec![foreign_path.clone()])
        .await
        .expect_err("not for this agent");
    assert!(matches!(error, Error::Package { .. }), "{error}");
    assert!(
        error.to_string().contains("allowance-foreign.json"),
        "{error}"
    );
    assert!(error.to_string().contains("not this agent's"), "{error}");

    // Either every package is imported or none is.
    let good = paper_package(&records, &[7], 3);
    let good_path = dir.path().join("public").join("allowance-good.json");
    store::write_public(&good_path, &good).expect("write the package");
    let missing = dir.path().join("public").join("allowance-missing.json");
    let error = import::packages(&files, vec![good_path, missing])
        .await
        .expect_err("a missing file");
    assert!(matches!(error, Error::Store(_)), "{error}");
    assert_eq!(
        files.read().await.expect("the store").allowances,
        saved.allowances
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_changes_of_the_store_never_lose_one_another() {
    let dir = tempdir().expect("a directory");
    let files = Files::new(dir.path().to_path_buf());
    files.init().await.expect("init");
    let records = files.read().await.expect("the store");
    let path = dir.path().join("public").join("allowance.json");
    store::write_public(&path, &paper_package(&records, &[50, 20, 20, 10], 1)).expect("write");
    import::packages(&files, vec![path]).await.expect("import");

    // Four writers, each with a handle of its own as four processes would have, each steadily
    // recording a newer transaction against its own voucher. If a change overwrote another's
    // with a store it had read before, a voucher would end with an older transaction.
    let writers: Vec<_> = (0..4)
        .map(|voucher| {
            let files = Files::new(dir.path().to_path_buf());
            tokio::spawn(async move {
                for round in 1..=25u8 {
                    files
                        .update(move |records| {
                            records.allowances[0].vouchers[voucher].txid = Some(TxID([round; 32]));
                            Ok(())
                        })
                        .await
                        .expect("update");
                }
            })
        })
        .collect();
    for writer in writers {
        writer.await.expect("a writer");
    }
    let saved = files.read().await.expect("the store");
    for voucher in &saved.allowances[0].vouchers {
        assert_eq!(voucher.txid, Some(TxID([25; 32])));
    }
}
