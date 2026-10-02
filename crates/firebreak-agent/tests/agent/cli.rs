//! The command line, run as the demonstration runs it.

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdout, Command, Output, Stdio};

use axum::http::StatusCode;
use firebreak_core::devnet::DENOMINATIONS;
use firebreak_core::store::AgentStore;
use serde_json::Value;

use crate::fixture::Devnet;
use crate::http::Client;

const AGENT: &str = env!("CARGO_BIN_EXE_firebreak-agent");

/// Runs the agent's command line, with the global options before the subcommand as the
/// demonstration passes them.
async fn run(data_dir: &Path, rpc: &str, args: &[&str]) -> Output {
    let mut command = vec![
        "--data-dir",
        data_dir.to_str().expect("UTF-8"),
        "--rpc",
        rpc,
    ];
    command.extend_from_slice(args);
    run_raw(&command).await
}

/// Runs the agent's command line with exactly `args`.
async fn run_raw(args: &[&str]) -> Output {
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    tokio::task::spawn_blocking(move || {
        Command::new(AGENT)
            .args(&args)
            .output()
            .expect("run the agent")
    })
    .await
    .expect("the command ran")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("UTF-8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("UTF-8")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_commands_of_the_demonstration_run_as_the_demonstration_runs_them() {
    let mut net = Devnet::start(true).await;
    let data = net.dir.path().to_path_buf();
    let rpc = net.node.url();
    let merchant = net.merchant();
    let mut outputs = Vec::new();

    // init makes the key and publishes its verification key, once.
    let output = run(&data, &rpc, &["init"]).await;
    assert!(output.status.success(), "{}", stderr(&output));
    let delegate = fs::read_to_string(data.join("public/delegate-key")).expect("the public key");
    let delegate = delegate.trim().to_owned();
    assert_eq!(delegate.len(), 64);
    assert!(stdout(&output).contains(&delegate));
    outputs.push(output);
    let output = run(&data, &rpc, &["init"]).await;
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("exists already"),
        "{}",
        stderr(&output)
    );
    outputs.push(output);
    let records = AgentStore::load(&data.join("agent/agent.json")).expect("the store");
    let secret = hex::encode(records.delegate_key.to_bytes());

    // The owner funds two allowances for the published key, and the agent imports both packages
    // with one command, then again.
    let first = net.fund(&records, &DENOMINATIONS, 0).await;
    let second = net.fund(&records, &[30, 30], 1).await;
    let paths = [first.write_package(&data), second.write_package(&data)];
    let names: Vec<&str> = paths
        .iter()
        .map(|path| path.to_str().expect("UTF-8"))
        .collect();
    let mut import = vec!["import"];
    import.extend_from_slice(&names);
    let output = run(&data, &rpc, &import).await;
    assert!(output.status.success(), "{}", stderr(&output));
    let expected = format!(
        "imported allowance {}: 4 vouchers worth 100 sparks for merchant {merchant}",
        first.allowance()
    );
    assert!(stdout(&output).contains(&expected), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("2 vouchers worth 60 sparks"),
        "{}",
        stdout(&output)
    );
    outputs.push(output);
    let output = run(&data, &rpc, &import).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).matches("was imported before").count(), 2);
    outputs.push(output);

    // status reconciles with the node, in text and as JSON.
    let output = run(&data, &rpc, &["status"]).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output).matches("unspent").count(),
        6,
        "{}",
        stdout(&output)
    );
    outputs.push(output);

    // With two allowances the one to pay from must be named.
    let output = run(
        &data,
        &rpc,
        &["pay", "--merchant", &merchant, "--amount", "60"],
    )
    .await;
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("2 allowances are imported"),
        "{}",
        stderr(&output)
    );
    outputs.push(output);

    // The payment of 60 redeems the 50 and the 10, and waits for the block.
    let id = first.allowance();
    let pay = [
        "pay",
        "--allowance",
        &id,
        "--merchant",
        &merchant,
        "--amount",
        "60",
        "--wait",
    ];
    let output = run(&data, &rpc, &pay).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("redeemed 60 sparks"),
        "{}",
        stdout(&output)
    );
    assert!(
        stdout(&output).contains("confirmed in block"),
        "{}",
        stdout(&output)
    );
    assert!(stdout(&output).contains("redeemed"), "{}", stdout(&output));
    outputs.push(output);

    // A payment the vouchers cannot make fails with the reason.
    let output = run(
        &data,
        &rpc,
        &[
            "pay",
            "--allowance",
            &id,
            "--merchant",
            &merchant,
            "--amount",
            "110",
        ],
    )
    .await;
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("insufficient authority"),
        "{}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("(stage: selection)"),
        "{}",
        stderr(&output)
    );
    outputs.push(output);
    let output = run(
        &data,
        &rpc,
        &[
            "pay",
            "--allowance",
            &id,
            "--merchant",
            &merchant,
            "--amount",
            "30",
        ],
    )
    .await;
    assert!(!output.status.success());
    assert!(stderr(&output).contains("no exact combination of vouchers for 30"));
    outputs.push(output);

    // An amount is plain digits, and the merchant is the allowance's.
    let output = run(
        &data,
        &rpc,
        &["pay", "--merchant", &merchant, "--amount", "6.5"],
    )
    .await;
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("plain non-negative integer"),
        "{}",
        stderr(&output)
    );
    outputs.push(output);
    let output = run(
        &data,
        &rpc,
        &[
            "pay",
            "--allowance",
            &id,
            "--merchant",
            "tf1elsewhere",
            "--amount",
            "20",
        ],
    )
    .await;
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("refusing to pay"),
        "{}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("(stage: policy)"),
        "{}",
        stderr(&output)
    );
    outputs.push(output);

    // The JSON status has the API's shape, and the global options may follow the subcommand.
    let output = run_raw(&[
        "status",
        "--json",
        "--data-dir",
        data.to_str().expect("UTF-8"),
        "--rpc",
        &rpc,
    ])
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    let status: Value = serde_json::from_str(&stdout(&output)).expect("JSON");
    assert_eq!(status["delegate"], delegate);
    assert!(status["tip_height"].is_u64());
    let [one, two] = status["allowances"]
        .as_array()
        .expect("allowances")
        .as_slice()
    else {
        panic!("expected two allowances in {status}");
    };
    assert_eq!(one["allowance"], first.allowance());
    assert_eq!(one["total"], "100");
    let states: Vec<&str> = one["vouchers"]
        .as_array()
        .expect("vouchers")
        .iter()
        .map(|voucher| voucher["state"].as_str().expect("a state"))
        .collect();
    assert_eq!(states, ["redeemed", "unspent", "unspent", "redeemed"]);
    assert!(one["vouchers"][0]["txid"].is_string());
    assert_eq!(two["total"], "60");
    outputs.push(output);

    // Nothing the command line printed holds the secret key.
    for output in &outputs {
        assert!(!stdout(output).contains(&secret));
        assert!(!stderr(output).contains(&secret));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serve_listens_where_it_is_told_and_gives_the_key_away_only_when_asked() {
    let net = Devnet::start(false).await;
    let data = net.dir.path();
    let rpc = net.node.url();
    let output = run(data, &rpc, &["init"]).await;
    assert!(output.status.success(), "{}", stderr(&output));
    let records = AgentStore::load(&data.join("agent/agent.json")).expect("the store");
    let secret = hex::encode(records.delegate_key.to_bytes());
    let public = hex::encode(records.public().to_bytes());

    // Without --reveal-key the key is not there, and the server says nothing of it.
    let quiet = Server::spawn(data, &rpc, &["--listen", "127.0.0.1:0"]);
    let client = Client::at(&quiet.base);
    let reply = client.get("/api/status").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.json["delegate"], public);
    assert_eq!(
        client.get("/api/delegated-key").await.status,
        StatusCode::NOT_FOUND
    );
    assert!(!quiet.banner.contains("DEMO ONLY"));

    // With it, on every interface: the server warns about both, and serves the key.
    let mut open = Server::spawn(data, &rpc, &["--listen", "0.0.0.0:0", "--reveal-key"]);
    let warnings = [open.stderr_line(), open.stderr_line()];
    assert!(
        warnings[0].contains("not a loopback address"),
        "{warnings:?}"
    );
    assert!(warnings[1].contains("DEMO ONLY"), "{warnings:?}");
    assert!(warnings[1].contains("SECRET key"), "{warnings:?}");
    assert!(!warnings.concat().contains(&secret));
    let client = Client::at(&open.base.replace("0.0.0.0", "127.0.0.1"));
    let reply = client.get("/api/delegated-key").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.json["secret"], secret);
    assert_eq!(reply.json["public"], public);
    assert!(!quiet.stop().contains(&secret));
}

/// A running `serve` command, which is stopped when this is dropped.
struct Server {
    child: Child,
    /// The URL the server said it listens on.
    base: String,
    /// The first line the server printed.
    banner: String,
    _stdout: BufReader<ChildStdout>,
    stderr: BufReader<ChildStderr>,
}

impl Server {
    /// Starts `serve` with `args` and waits for its banner.
    fn spawn(data_dir: &Path, rpc: &str, args: &[&str]) -> Server {
        let mut child = Command::new(AGENT)
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--rpc")
            .arg(rpc)
            .arg("serve")
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start the server");
        let mut stdout = BufReader::new(child.stdout.take().expect("a stdout pipe"));
        let stderr = BufReader::new(child.stderr.take().expect("a stderr pipe"));
        let mut banner = String::new();
        stdout.read_line(&mut banner).expect("the banner");
        let base = banner
            .split_whitespace()
            .find(|word| word.starts_with("http://"))
            .expect("an address in the banner")
            .to_owned();
        Server {
            child,
            base,
            banner,
            _stdout: stdout,
            stderr,
        }
    }

    /// The next line the server wrote to its error output.
    fn stderr_line(&mut self) -> String {
        let mut line = String::new();
        self.stderr.read_line(&mut line).expect("a line");
        line
    }

    /// Stops the server, and gives what it wrote to its error output that was not read yet.
    fn stop(mut self) -> String {
        // A server that is gone already is as stopped as one that was just killed.
        let _ = self.child.kill();
        let _ = self.child.wait();
        let mut rest = String::new();
        self.stderr
            .read_to_string(&mut rest)
            .expect("the rest of the error output");
        rest
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // The server is meant to be gone; whether it already is does not matter.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
