//! `firebreak-attack`: the adversary's command line.
//!
//! It holds what Firebreak's threat model gives an adversary and nothing more: the delegated key
//! (as the demo reveals it, or from the agent's key file) and the public delegation packages. It
//! builds its candidates itself and submits them directly to the node, bypassing the agent and
//! its policy checks, and reports exactly where each one was stopped.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use curve25519_dalek::scalar::Scalar as DalekScalar;
use firebreak_attack::run::{self, Context, Report, Verdict};
use firebreak_attack::{ALL, Adversary, Attack};
use firebreak_core::store::{self, AgentStore, DelegationPackage, serde_hex};
use firebreak_core::{Chain, NETWORK, Voucher, keys};
use flamekd::util;
use flamepayments::Account;
use serde::{Deserialize, Serialize};

#[derive(Parser)]
#[command(
    name = "firebreak-attack",
    about = "Attack Firebreak vouchers with the delegated key, directly against the node"
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
    /// Lists the attacks.
    List,
    /// Runs one attack and reports where it was stopped.
    Attempt {
        /// The attack, as `list` names it.
        name: String,
        /// The voucher to attack, by contract ID. Default: the first one the attack can use.
        #[arg(long)]
        voucher: Option<String>,
        #[command(flatten)]
        material: Material,
    },
    /// Runs every attack that must be refused against one unspent voucher, and tabulates them.
    Matrix {
        #[command(flatten)]
        material: Material,
    },
}

/// What the adversary obtained.
#[derive(clap::Args)]
struct Material {
    /// The delegated secret key in hex, as the demo reveals it. Default: the agent's key file.
    #[arg(long)]
    key: Option<String>,
    /// A public delegation package. Default: every `allowance-*.json` in the public directory.
    #[arg(long)]
    package: Vec<PathBuf>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match execute(cli).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn execute(cli: Cli) -> Result<ExitCode, Box<dyn std::error::Error>> {
    match cli.command {
        Command::List => {
            for attack in ALL {
                println!("{:<18} {}", attack.name(), attack.description());
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Attempt {
            name,
            voucher,
            material,
        } => {
            let attack = Attack::from_name(&name)
                .ok_or_else(|| format!("no attack {name:?}; `list` names them"))?;
            let target = voucher.as_deref().map(parse_id).transpose()?;
            let context = context(&cli.data_dir, &cli.rpc, &material)?;
            let report = run::attempt(&context, attack, target).await?;
            print_report(&report);
            Ok(exit_code(&[report]))
        }
        Command::Matrix { material } => {
            let context = context(&cli.data_dir, &cli.rpc, &material)?;
            let mut reports = Vec::new();
            for attack in ALL {
                if attack.is_valid_payment() || attack == Attack::RedeemSpent {
                    continue;
                }
                reports.push(run::attempt(&context, attack, None).await?);
            }
            match run::attempt(&context, Attack::RedeemSpent, None).await {
                Ok(report) => reports.push(report),
                Err(error) => println!("redeem-spent skipped: {error}"),
            }
            print_table(&reports);
            Ok(exit_code(&reports))
        }
    }
}

/// The adversary's context: the node, its keys, the vouchers it knows of, and the journal.
fn context(
    data_dir: &Path,
    rpc: &str,
    material: &Material,
) -> Result<Context, Box<dyn std::error::Error>> {
    let delegate_key = match &material.key {
        Some(key) => parse_key(key)?,
        None => {
            let path = data_dir.join("agent").join("agent.json");
            println!("using the delegated key held in {}", path.display());
            AgentStore::load(&path)?.delegate_key
        }
    };
    let packages = if material.package.is_empty() {
        public_packages(&data_dir.join("public"))?
    } else {
        material.package.clone()
    };
    let mut vouchers: Vec<Voucher> = Vec::new();
    for path in &packages {
        let package: DelegationPackage = store::read(path)?;
        vouchers.extend(package.vouchers()?);
    }
    if vouchers.is_empty() {
        return Err("no public delegation package names any voucher".into());
    }
    let adversary = Adversary {
        delegate_key,
        address: attacker(&data_dir.join("attacker").join("attacker.json"))?,
    };
    println!("attacker address {}", adversary.address.to_bech32(NETWORK));
    Ok(Context {
        chain: Chain::connect(rpc)?,
        adversary,
        vouchers,
        journal: Some(data_dir.join("public").join("journal.jsonl")),
    })
}

/// Every delegation package in the public directory.
fn public_packages(dir: &Path) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let mut packages: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("allowance-") && name.ends_with(".json"))
        })
        .collect();
    packages.sort();
    Ok(packages)
}

/// The adversary's own wallet, which it wants the vouchers' value paid to.
#[derive(Serialize, Deserialize)]
struct Attacker {
    #[serde(with = "serde_hex")]
    seed: [u8; 64],
}

/// The adversary's own address, from the seed saved at `path`, which is created on first use.
fn attacker(path: &Path) -> Result<flamekd::ReceivingAddress, Box<dyn std::error::Error>> {
    let attacker = match store::read::<Attacker>(path) {
        Ok(attacker) => attacker,
        Err(error) if error.is_not_found() => {
            let attacker = Attacker {
                seed: keys::random_bytes(&mut rand::rngs::OsRng),
            };
            store::write_private(path, &attacker)?;
            attacker
        }
        Err(error) => return Err(error.into()),
    };
    let account = Account::from_seed(&attacker.seed, NETWORK, 0)?;
    Ok(account.address_at(util::RECEIVING, 0)?)
}

/// A 32-byte contract ID written in hex.
fn parse_id(text: &str) -> Result<[u8; 32], String> {
    let bytes = hex::decode(text.trim()).map_err(|error| format!("voucher id: {error}"))?;
    bytes
        .try_into()
        .map_err(|_| "a voucher id is 32 bytes".to_owned())
}

/// A secret key written in hex, as the demo reveals it.
fn parse_key(text: &str) -> Result<DalekScalar, String> {
    let bytes: [u8; 32] = hex::decode(text.trim())
        .map_err(|error| format!("key: {error}"))?
        .try_into()
        .map_err(|_| "a key is 32 bytes".to_owned())?;
    Option::from(DalekScalar::from_canonical_bytes(bytes))
        .ok_or_else(|| "the key is not a canonical scalar".to_owned())
}

fn print_report(report: &Report) {
    println!(
        "attack {}: {}",
        report.attack.name(),
        report.attack.description()
    );
    println!(
        "  target voucher  {} ({} sparks, {})",
        hex::encode(report.target),
        report.target_qty,
        report.before
    );
    match &report.verdict {
        Verdict::Prover(error) => {
            println!("  prover          REFUSED: {error}");
            println!("                  (no transaction bytes exist to submit)");
        }
        Verdict::Signer(error) => println!("  signer          REFUSED: {error}"),
        Verdict::Node { verifier, error } => {
            println!("  signed with     the delegated key");
            print_verifier(verifier.as_deref());
            println!("  node            REFUSED: {error}");
        }
        Verdict::Unknown { verifier, error } => {
            print_verifier(verifier.as_deref());
            println!("  node            NO ANSWER: {error}");
        }
        Verdict::Accepted { txid, verifier } => {
            print_verifier(verifier.as_deref());
            println!(
                "  node            ADMITTED transaction {}",
                hex::encode(txid.0)
            );
        }
    }
    println!("  voucher after   {}", report.after);
    if report.is_breach() {
        println!("=> SECURITY FAILURE: the node admitted an unauthorized spend");
    } else if report.as_expected() {
        println!("=> {}", conclusion(report));
    } else {
        println!("=> unexpected outcome");
    }
}

fn print_verifier(verifier: Option<&str>) {
    match verifier {
        Some(error) => println!("  local verifier  REFUSED: {error}"),
        None => println!("  local verifier  accepted the bytes"),
    }
}

fn conclusion(report: &Report) -> &'static str {
    match report.verdict {
        Verdict::Prover(_) => "stopped: the VM cannot even prove it",
        Verdict::Signer(_) => "stopped: it cannot be signed",
        Verdict::Node { .. } => "stopped: the node refused it",
        Verdict::Accepted { .. } => "admitted, as a valid payment to the merchant must be",
        Verdict::Unknown { .. } => "unknown",
    }
}

fn print_table(reports: &[Report]) {
    println!();
    println!(
        "{:<18} {:<10} {:<40} voucher after",
        "attack", "stopped by", "error"
    );
    for report in reports {
        let (stage, error) = match &report.verdict {
            Verdict::Prover(error) => ("prover", error.as_str()),
            Verdict::Signer(error) => ("signer", error.as_str()),
            Verdict::Node { error, .. } => ("node", error.as_str()),
            Verdict::Unknown { error, .. } => ("unknown", error.as_str()),
            Verdict::Accepted { .. } => ("ADMITTED", ""),
        };
        let error: String = error.chars().take(40).collect();
        println!(
            "{:<18} {:<10} {:<40} {}",
            report.attack.name(),
            stage,
            error,
            report.after
        );
    }
}

/// 0 when every outcome is the predicted one, 2 when the node admitted an unauthorized spend,
/// 1 otherwise.
fn exit_code(reports: &[Report]) -> ExitCode {
    if reports.iter().any(Report::is_breach) {
        ExitCode::from(2)
    } else if reports.iter().all(Report::as_expected) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
