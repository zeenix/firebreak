//! The program itself: its arguments, what it prints, and how it exits.
//!
//! The commands run exactly as the demonstration's script runs them, against the node of the
//! test, with the global options before the subcommand.

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

use crate::harness::Harness;

/// Runs `firebreak-merchant --data-dir <data> --rpc <rpc> <args>` and waits for it to finish.
async fn merchant(data: &Path, rpc: &str, args: &[&str]) -> Output {
    let (data, rpc) = (data.to_owned(), rpc.to_owned());
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_firebreak-merchant"))
            .arg("--data-dir")
            .arg(&data)
            .arg("--rpc")
            .arg(&rpc)
            .args(args)
            .output()
            .expect("run the program")
    })
    .await
    .expect("the program runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("text on stdout")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("text on stderr")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_demonstrations_commands_run_as_written() {
    // `init` needs no node, and publishes the address the owner funds vouchers for.
    let fresh = TempDir::new().expect("a temporary directory");
    let init = merchant(fresh.path(), "http://127.0.0.1:1", &["init"]).await;
    assert!(init.status.success(), "{}", stderr(&init));
    let published = std::fs::read_to_string(fresh.path().join("public/merchant-address"))
        .expect("the published address");
    assert!(
        published.starts_with("tf1") && published.ends_with('\n'),
        "{published}"
    );
    assert!(
        stdout(&init).contains(published.trim()),
        "{}",
        stdout(&init)
    );

    // With payments made, `inspect` lists them and `inspect --json` is the snapshot.
    let harness = Harness::start(true).await;
    let (data, rpc) = (harness.dir.path(), harness.node.url());
    let funded = harness.fund(&[50, 20, 20, 10]).await;
    let vouchers = harness.vouchers(&funded.allowance);
    harness.pay(&[&vouchers[0], &vouchers[3]]).await;

    let listed = merchant(data, &rpc, &["inspect"]).await;
    assert!(listed.status.success(), "{}", stderr(&listed));
    let shown = stdout(&listed);
    assert!(
        shown.contains("balance: 60 sparks") && shown.contains("firebreak voucher"),
        "{shown}"
    );

    let json = merchant(data, &rpc, &["inspect", "--json"]).await;
    assert!(json.status.success(), "{}", stderr(&json));
    let value: Value = serde_json::from_str(&stdout(&json)).expect("JSON and nothing else");
    assert_eq!(value["balance"], "60");
    assert_eq!(value["receipts"].as_array().map(Vec::len), Some(2));

    // `spend --wait` moves both payments, and the balance stays what it was.
    let spent = merchant(data, &rpc, &["spend", "--wait"]).await;
    assert!(spent.status.success(), "{}", stderr(&spent));
    let shown = stdout(&spent);
    assert!(
        shown.contains("spent 60 sparks") && shown.contains("confirmed in block"),
        "{shown}"
    );
    let json = merchant(data, &rpc, &["inspect", "--json"]).await;
    let value: Value = serde_json::from_str(&stdout(&json)).expect("JSON");
    assert_eq!(value["balance"], "60");
    assert!(
        value["receipts"]
            .as_array()
            .expect("receipts")
            .iter()
            .all(|r| r["spent"] == true)
    );
    assert_eq!(value["spends"][0]["state"], "confirmed");

    // There is nothing left to spend, which is not an error.
    let again = merchant(data, &rpc, &["spend"]).await;
    assert!(again.status.success(), "{}", stderr(&again));
    assert!(
        stdout(&again).contains("nothing to spend"),
        "{}",
        stdout(&again)
    );
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failures_exit_with_an_error_that_names_the_stage() {
    let harness = Harness::start(false).await;
    let data = harness.dir.path();

    // A node that does not answer: the error says the outcome is unknown, and nothing changed.
    let silent = merchant(data, "http://127.0.0.1:1", &["inspect"]).await;
    assert_eq!(silent.status.code(), Some(1));
    assert!(stdout(&silent).is_empty(), "{}", stdout(&silent));
    assert!(
        stderr(&silent).starts_with("error: unknown:"),
        "{}",
        stderr(&silent)
    );
    assert!(
        stderr(&silent).contains("outcome is unknown"),
        "{}",
        stderr(&silent)
    );

    // A second `init` does not replace the wallet.
    let again = merchant(data, &harness.node.url(), &["init"]).await;
    assert_eq!(again.status.code(), Some(1));
    assert!(
        stderr(&again).contains("already exists"),
        "{}",
        stderr(&again)
    );

    // A wallet that was never created.
    let empty = TempDir::new().expect("a temporary directory");
    let missing = merchant(empty.path(), &harness.node.url(), &["inspect"]).await;
    assert_eq!(missing.status.code(), Some(1));
    assert!(
        stderr(&missing).contains("firebreak-merchant init"),
        "{}",
        stderr(&missing)
    );
    harness.finish().await;
}
