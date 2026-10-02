//! `firebreak-merchant`: the merchant's command line.
//!
//! It creates the merchant's wallet, finds the payments that vouchers make to it, opens their
//! receipts, and spends what it received. This program only reads its arguments and prints; the
//! work is in the library.

use std::error;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use firebreak_core::Chain;
use firebreak_merchant::{Context, Error, Files, init, inspect, spend};

#[derive(Parser)]
#[command(
    name = "firebreak-merchant",
    about = "Receive and spend Firebreak payments as the merchant"
)]
struct Cli {
    /// The data directory.
    #[arg(long, default_value = ".firebreak", global = true)]
    data_dir: PathBuf,
    /// The node's JSON-RPC address.
    #[arg(long, default_value = "http://127.0.0.1:7740", global = true)]
    rpc: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Creates the merchant's wallet and publishes its address.
    Init,
    /// Lists the payments received, with their receipts opened.
    Inspect {
        /// Prints the snapshot written to merchant-status.json instead of a summary.
        #[arg(long)]
        json: bool,
    },
    /// Spends every unspent payment to a fresh address of the merchant's own.
    Spend {
        /// Waits up to 60 seconds for the spending transaction to confirm.
        #[arg(long)]
        wait: bool,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match execute(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn execute(cli: Cli) -> Result<(), Box<dyn error::Error>> {
    let Cli {
        data_dir,
        rpc,
        command,
    } = cli;
    match command {
        Command::Init => println!("{}", init(&Files::new(data_dir))?),
        Command::Inspect { json } => {
            let report = inspect(&context(data_dir, &rpc)?).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report.status)?);
            } else {
                println!("{report}");
            }
        }
        Command::Spend { wait } => println!("{}", spend(&context(data_dir, &rpc)?, wait).await?),
    }
    Ok(())
}

/// The files under `data_dir` and the node at `rpc`, which is only contacted when asked.
fn context(data_dir: PathBuf, rpc: &str) -> Result<Context, Error> {
    let chain = Chain::connect(rpc).map_err(Error::Node)?;
    Ok(Context::new(data_dir, chain))
}
