//! The private files: who may read them, and who may change them at the same time.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::thread;
use std::time::Duration;

use firebreak_owner::{Error, Files, init};
use tempfile::TempDir;

use crate::harness::Harness;

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

fn wallet() -> (TempDir, Files) {
    let dir = TempDir::new().expect("a temporary directory");
    let files = Files::new(dir.path().join("data"));
    init(&files, 1_000).expect("init");
    (dir, files)
}

#[test]
fn updates_by_different_threads_never_lose_each_other() {
    let (_dir, files) = wallet();
    thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                for _ in 0..10 {
                    files
                        .update(|store| {
                            store.next_change_index += 1;
                            Ok(())
                        })
                        .expect("update");
                }
            });
        }
    });
    assert_eq!(files.load().expect("the store").next_change_index, 80);
}

#[test]
fn an_update_reads_the_store_only_after_it_has_the_lock() {
    let (_dir, files) = wallet();
    thread::scope(|scope| {
        // The first update holds the lock while it works.
        scope.spawn(|| {
            files
                .update(|store| {
                    thread::sleep(Duration::from_millis(400));
                    store.next_change_index = 7;
                    Ok(())
                })
                .expect("the slow update");
        });
        thread::sleep(Duration::from_millis(100));
        // The second has to wait for it, and then sees what it wrote.
        files
            .update(|store| {
                assert_eq!(store.next_change_index, 7);
                store.next_change_index += 1;
                Ok(())
            })
            .expect("the update that waited");
    });
    assert_eq!(files.load().expect("the store").next_change_index, 8);
}

#[test]
fn an_update_that_fails_saves_nothing() {
    let (_dir, files) = wallet();
    let before = fs::read(files.store()).expect("the store");
    let error = files
        .update(|store| {
            store.next_change_index = 99;
            Err::<(), _>(Error::Refused("no".to_owned()))
        })
        .expect_err("the change fails");
    assert!(matches!(error, Error::Refused(_)));
    assert_eq!(fs::read(files.store()).expect("the store"), before);
}

#[test]
fn private_files_stay_private_through_updates() {
    let (_dir, files) = wallet();
    for _ in 0..3 {
        files
            .update(|store| {
                store.next_change_index += 1;
                Ok(())
            })
            .expect("update");
    }
    assert_eq!(mode(&files.store()), 0o600);
    assert_eq!(mode(&files.lock()), 0o600);
    assert_eq!(mode(files.store().parent().expect("a directory")), 0o700);
    let leftovers: Vec<_> = fs::read_dir(files.store().parent().expect("a directory"))
        .expect("the directory")
        .map(|entry| entry.expect("an entry").file_name())
        .collect();
    assert_eq!(
        leftovers.len(),
        2,
        "only the store and the lock: {leftovers:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn what_the_commands_write_is_private_or_public_as_it_should_be() {
    let harness = Harness::start(true).await;
    let funded = firebreak_owner::create_allowance(&harness.ctx, &harness.request(&[50, 20], true))
        .await
        .expect("fund the allowance");

    // The secrets are for the owner alone.
    assert_eq!(mode(&harness.files().store()), 0o600);
    assert_eq!(mode(&harness.files().lock()), 0o600);
    assert_eq!(
        mode(harness.files().store().parent().expect("a directory")),
        0o700
    );

    // The package, the snapshot and the journal are for everyone: the delegate, the dashboard
    // and the public.
    for path in [
        funded.package,
        harness.files().status(),
        harness.files().journal(),
    ] {
        assert_eq!(
            mode(&path) & 0o044,
            0o044,
            "{} is readable by others",
            path.display()
        );
    }
    let secret = harness.files().load().expect("the store").wallet_seed;
    for path in [harness.files().status(), harness.files().journal()] {
        let text = fs::read_to_string(&path).expect("a public file");
        assert!(!text.contains(&hex::encode(secret)), "{}", path.display());
    }
    harness.finish().await;
}
