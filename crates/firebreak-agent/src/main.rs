//! The `firebreak-agent` command line: the delegated app that pays one merchant out of an
//! allowance.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use firebreak_agent::{Agent, Files, PayRequest, Payment, api, import};
use firebreak_core::store::{Import, VoucherState, serde_amount};
use firebreak_core::{Chain, NETWORK};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("firebreak-agent: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let Cli {
        data_dir,
        rpc,
        command,
    } = cli;
    let files = Files::new(data_dir);
    match command {
        Command::Init => init(&files).await,
        Command::Import { packages } => import_packages(&files, packages).await,
        Command::Status { json } => status(&agent(files, &rpc)?, json).await,
        Command::Pay {
            allowance,
            merchant,
            amount,
            wait,
        } => {
            let request = PayRequest {
                allowance,
                merchant,
                amount,
            };
            pay(&agent(files, &rpc)?, &request, wait).await
        }
        Command::Serve { listen, reveal_key } => {
            serve(agent(files, &rpc)?, listen, reveal_key, &rpc).await
        }
    }
}

async fn init(files: &Files) -> Result<(), String> {
    let public = files.init().await.map_err(|error| error.to_string())?;
    println!("delegate key (public): {public}");
    println!("private store: {}", files.agent_json().display());
    println!("public key file: {}", files.delegate_key().display());
    Ok(())
}

async fn import_packages(files: &Files, paths: Vec<PathBuf>) -> Result<(), String> {
    let imported = import::packages(files, paths)
        .await
        .map_err(|error| error.to_string())?;
    for package in imported {
        match package.outcome {
            Import::Added => println!(
                "imported allowance {}: {} vouchers worth {} sparks for merchant {}",
                package.allowance,
                package.vouchers,
                package.total,
                package.merchant.to_bech32(NETWORK)
            ),
            Import::AlreadyImported => {
                println!("allowance {} was imported before", package.allowance);
            }
        }
    }
    Ok(())
}

async fn status(agent: &Agent, json: bool) -> Result<(), String> {
    let status = agent.status().await.map_err(|error| error.to_string())?;
    if json {
        let text = serde_json::to_string_pretty(&status).map_err(|error| error.to_string())?;
        println!("{text}");
    } else {
        print!("{status}");
    }
    Ok(())
}

async fn pay(agent: &Agent, request: &PayRequest, wait: bool) -> Result<(), String> {
    let limit = wait.then_some(WAIT);
    let payment = agent
        .pay(request, limit)
        .await
        .map_err(|error| format!("{} (stage: {})", error.message, error.stage))?;
    print_payment(request, &payment);
    for warning in &payment.warnings {
        eprintln!("warning: {warning}");
    }
    if wait && payment.height.is_none() {
        return Err(format!(
            "the redemption was submitted but no block confirmed it within {} seconds; its \
             vouchers stay redemption_pending, and `status` shows how it ends",
            WAIT.as_secs()
        ));
    }
    Ok(())
}

fn print_payment(request: &PayRequest, payment: &Payment) {
    let verb = match payment.state {
        VoucherState::Redeemed => "redeemed",
        _ => "submitted a redemption of",
    };
    println!(
        "{verb} {} sparks for {} in transaction {}",
        payment.amount(),
        request.merchant,
        hex::encode(payment.txid.0)
    );
    for voucher in &payment.vouchers {
        println!("  {:>10} sparks  {}", voucher.qty, hex::encode(voucher.id));
    }
    match payment.height {
        Some(height) => println!("confirmed in block {height}: {}", payment.state),
        None => println!("state: {}", payment.state),
    }
}

async fn serve(
    agent: Agent,
    listen: SocketAddr,
    reveal_key: bool,
    rpc: &str,
) -> Result<(), String> {
    // Fail now, not on the first request, when there is no agent to serve.
    agent
        .files()
        .read()
        .await
        .map_err(|error| error.to_string())?;
    let listener = TcpListener::bind(listen)
        .await
        .map_err(|error| format!("cannot listen on {listen}: {error}"))?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("cannot read the listening address: {error}"))?;
    if !address.ip().is_loopback() {
        eprintln!(
            "warning: {address} is not a loopback address; anyone who can reach it can make the \
             agent pay"
        );
    }
    println!("LOCAL DEVNET payment API on http://{address} (node {rpc})");
    if reveal_key {
        eprintln!(
            "DEMO ONLY: GET http://{address}/api/delegated-key gives the delegated SECRET key to \
             anyone who asks"
        );
    }
    axum::serve(listener, api::router(Arc::new(agent), reveal_key))
        .await
        .map_err(|error| format!("the server stopped: {error}"))
}

/// The agent whose files are `files`, talking to the node at `rpc`.
fn agent(files: Files, rpc: &str) -> Result<Agent, String> {
    let chain = Chain::connect(rpc).map_err(|error| error.to_string())?;
    Ok(Agent::new(files, chain))
}

/// An amount of sparks written as plain digits.
fn parse_amount(text: &str) -> Result<u64, String> {
    serde_amount::parse(text).map_err(|error| error.to_string())
}

/// The Firebreak payment agent: the delegated app that pays one merchant out of an allowance.
#[derive(Debug, Parser)]
#[command(version)]
struct Cli {
    /// The data directory the Firebreak roles share.
    #[arg(long, global = true, default_value = ".firebreak")]
    data_dir: PathBuf,

    /// The Flame node's JSON-RPC URL.
    #[arg(long, global = true, default_value = "http://127.0.0.1:7740")]
    rpc: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Creates the delegated key and the agent's private store.
    Init,

    /// Serves the payment API over HTTP.
    Serve {
        /// The address to listen on.
        #[arg(long, default_value = "127.0.0.1:7741")]
        listen: SocketAddr,

        /// DEMO ONLY: serve the delegated secret key at GET /api/delegated-key.
        #[arg(long)]
        reveal_key: bool,
    },

    /// Imports the delegation packages an owner wrote.
    Import {
        /// The package files, such as `allowance-<id>.json`.
        #[arg(required = true)]
        packages: Vec<PathBuf>,
    },

    /// Shows the allowances and the state of every voucher.
    Status {
        /// Print JSON, as `GET /api/status` answers.
        #[arg(long)]
        json: bool,
    },

    /// Pays a merchant by redeeming vouchers whose values add up to the amount.
    Pay {
        /// The allowance to pay from; the only one imported when left out.
        #[arg(long)]
        allowance: Option<String>,

        /// The merchant's address, which must be the allowance's merchant.
        #[arg(long)]
        merchant: String,

        /// The amount in sparks.
        #[arg(long, value_parser = parse_amount)]
        amount: u64,

        /// Wait up to 60 seconds for a block to confirm the redemption, and fail if none does.
        #[arg(long)]
        wait: bool,
    },
}

/// How long `pay --wait` waits for the redemption to be confirmed.
const WAIT: Duration = Duration::from_secs(60);
