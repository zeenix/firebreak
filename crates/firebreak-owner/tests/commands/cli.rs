//! The program itself: its arguments, what it prints, and how it exits.
//!
//! The commands run exactly as the demonstration's script runs them, against the node of the
//! test, with the global options before the subcommand.

use std::path::Path;
use std::process::{Command, Output};

use firebreak_core::NETWORK;
use serde_json::Value;
use tempfile::TempDir;

use crate::harness::Harness;

/// Runs `firebreak-owner --data-dir <data> --rpc <rpc> <args>` and waits for it to finish.
async fn owner(data: &Path, rpc: &str, args: &[&str]) -> Output {
    let (data, rpc) = (data.to_owned(), rpc.to_owned());
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_firebreak-owner"))
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
    // `init` runs before any node exists, and writes what the node starts from.
    let dir = TempDir::new().expect("a temporary directory");
    let init = owner(
        dir.path(),
        "http://127.0.0.1:1",
        &["init", "--genesis-sparks", "1000"],
    )
    .await;
    assert!(init.status.success(), "{}", stderr(&init));
    assert!(
        stdout(&init).contains("created the owner's wallet"),
        "{}",
        stdout(&init)
    );
    let harness = Harness::adopt(dir, true).await;
    let (data, rpc) = (harness.dir.path(), harness.node.url());

    // `status`: the wallet holds the genesis allocation.
    let status = owner(data, &rpc, &["status"]).await;
    assert!(status.status.success(), "{}", stderr(&status));
    assert!(
        stdout(&status).starts_with("wallet: 1000 sparks"),
        "{}",
        stdout(&status)
    );
    assert!(
        stdout(&status).contains("no allowances"),
        "{}",
        stdout(&status)
    );

    // `create-allowance --wait`, with the merchant's and the delegate's public values.
    let merchant = harness.merchant.to_bech32(NETWORK);
    let delegate = hex::encode(harness.delegate().to_bytes());
    let created = owner(
        data,
        &rpc,
        &[
            "create-allowance",
            "--merchant",
            &merchant,
            "--delegate",
            &delegate,
            "--vouchers",
            "50,20,20,10",
            "--wait",
        ],
    )
    .await;
    assert!(created.status.success(), "{}", stderr(&created));
    let shown = stdout(&created);
    assert!(
        shown.contains("confirmed in block") && shown.contains("50 sparks"),
        "{shown}"
    );
    assert!(shown.contains("delegation package"), "{shown}");
    let packages: Vec<_> = std::fs::read_dir(data.join("public"))
        .expect("the public directory")
        .map(|entry| {
            entry
                .expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.starts_with("allowance-") && name.ends_with(".json"))
        .collect();
    assert_eq!(
        packages.len(),
        1,
        "one package for the delegate: {packages:?}"
    );

    // `status --json`: only JSON on stdout, in the shape the demonstration's `jq` reads.
    let json = owner(data, &rpc, &["status", "--json"]).await;
    assert!(json.status.success(), "{}", stderr(&json));
    let value: Value = serde_json::from_str(&stdout(&json)).expect("JSON and nothing else");
    assert_eq!(value["wallet"]["balance"], "900");
    let states: Vec<&str> = value["allowances"][0]["vouchers"]
        .as_array()
        .expect("vouchers")
        .iter()
        .map(|voucher| voucher["state"].as_str().expect("a state"))
        .collect();
    assert_eq!(states, ["unspent"; 4]);

    // `reclaim --wait` takes everything back, and says so only once it confirmed.
    let reclaimed = owner(data, &rpc, &["reclaim", "--wait"]).await;
    assert!(reclaimed.status.success(), "{}", stderr(&reclaimed));
    assert!(
        stdout(&reclaimed).contains("revoked"),
        "{}",
        stdout(&reclaimed)
    );
    let json = owner(data, &rpc, &["status", "--json"]).await;
    let value: Value = serde_json::from_str(&stdout(&json)).expect("JSON");
    assert_eq!(value["wallet"]["balance"], "1000");
    let unspent = value["allowances"][0]["vouchers"]
        .as_array()
        .expect("vouchers")
        .iter()
        .filter(|voucher| voucher["state"] == "unspent")
        .count();
    assert_eq!(unspent, 0);
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failures_exit_with_an_error_that_names_the_stage() {
    let harness = Harness::start(false).await;
    let data = harness.dir.path();
    let rpc = harness.node.url();
    let merchant = harness.merchant.to_bech32(NETWORK);
    let delegate = hex::encode(harness.delegate().to_bytes());

    // A request that is refused: nothing was done, and the exit code says so.
    let refused = owner(
        data,
        &rpc,
        &[
            "create-allowance",
            "--merchant",
            &merchant,
            "--delegate",
            &delegate,
            "--vouchers",
            "50,0",
        ],
    )
    .await;
    assert_eq!(refused.status.code(), Some(1));
    assert!(stdout(&refused).is_empty(), "{}", stdout(&refused));
    assert!(
        stderr(&refused).starts_with("error: refused: voucher amount"),
        "{}",
        stderr(&refused)
    );

    // A node that does not answer: the error says the outcome is unknown, and nothing changed.
    let silent = owner(data, "http://127.0.0.1:1", &["status"]).await;
    assert_eq!(silent.status.code(), Some(1));
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
    let again = owner(data, &rpc, &["init"]).await;
    assert_eq!(again.status.code(), Some(1));
    assert!(
        stderr(&again).contains("already exists"),
        "{}",
        stderr(&again)
    );

    // A wallet that was never created.
    let empty = TempDir::new().expect("a temporary directory");
    let missing = owner(empty.path(), &rpc, &["status"]).await;
    assert_eq!(missing.status.code(), Some(1));
    assert!(
        stderr(&missing).contains("firebreak-owner init"),
        "{}",
        stderr(&missing)
    );
    harness.finish().await;
}
