//! `firebreak-owner`: the owner's command line.
//!
//! It creates the owner's wallet, funds allowances of single-use vouchers for a merchant and a
//! delegate, takes back the vouchers the delegate has not redeemed, and shows where everything
//! stands. This program only reads its arguments and prints; the work is in the library.

use std::error;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use firebreak_core::Chain;
use firebreak_owner::{
    Context, CreateAllowance, DEFAULT_GENESIS_SPARKS, Error, Files, Reclaim, create_allowance,
    init, parse_delegate, parse_merchant, parse_vouchers, reclaim, status,
};

#[derive(Parser)]
#[command(
    name = "firebreak-owner",
    about = "Fund and recover Firebreak allowances as the owner"
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
    /// Creates the owner's wallet and the devnet genesis that funds it.
    Init {
        /// What the genesis gives the owner's first address, in sparks.
        #[arg(long, default_value_t = DEFAULT_GENESIS_SPARKS)]
        genesis_sparks: u64,
    },
    /// Funds an allowance: one single-use voucher for each amount, payable to one merchant.
    CreateAllowance {
        /// The merchant's address, which every voucher pays.
        #[arg(long)]
        merchant: String,
        /// The delegate's verification key in hex, which may redeem the vouchers.
        #[arg(long)]
        delegate: String,
        /// The vouchers' amounts in sparks, comma separated, such as 50,20,20,10.
        #[arg(long)]
        vouchers: String,
        /// Waits up to 60 seconds for the funding transaction to confirm.
        #[arg(long)]
        wait: bool,
    },
    /// Takes back the vouchers the delegate has not redeemed.
    Reclaim {
        /// Only this allowance's vouchers, by id.
        #[arg(long)]
        allowance: Option<String>,
        /// Only this voucher, by contract id or the start of it. Repeat it for more.
        #[arg(long = "voucher", value_delimiter = ',')]
        vouchers: Vec<String>,
        /// Waits up to 60 seconds for each recovery transaction to confirm.
        #[arg(long)]
        wait: bool,
    },
    /// Shows the wallet, the allowances and the vouchers as the node confirms them.
    Status {
        /// Prints the snapshot written to owner-status.json instead of a summary.
        #[arg(long)]
        json: bool,
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
        Command::Init { genesis_sparks } => {
            println!("{}", init(&Files::new(data_dir), genesis_sparks)?);
        }
        Command::CreateAllowance {
            merchant,
            delegate,
            vouchers,
            wait,
        } => {
            let request = CreateAllowance {
                merchant: parse_merchant(&merchant)?,
                delegate: parse_delegate(&delegate)?,
                vouchers: parse_vouchers(&vouchers)?,
                wait,
            };
            let ctx = context(data_dir, &rpc)?;
            println!("{}", create_allowance(&ctx, &request).await?);
        }
        Command::Reclaim {
            allowance,
            vouchers,
            wait,
        } => {
            let request = Reclaim {
                allowance,
                vouchers,
                wait,
            };
            let ctx = context(data_dir, &rpc)?;
            println!("{}", reclaim(&ctx, &request).await?);
        }
        Command::Status { json } => {
            let ctx = context(data_dir, &rpc)?;
            let report = status(&ctx).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report.status)?);
            } else {
                println!("{report}");
            }
        }
    }
    Ok(())
}

/// The files under `data_dir` and the node at `rpc`, which is only contacted when asked.
fn context(data_dir: PathBuf, rpc: &str) -> Result<Context, Error> {
    let chain = Chain::connect(rpc).map_err(Error::Node)?;
    Ok(Context::new(data_dir, chain))
}
