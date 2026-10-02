//! `firebreak-mcp`: the Firebreak payment agent as an MCP server on standard input and output.
//!
//! Standard output carries only MCP messages; everything the server has to say goes to standard
//! error.

use std::process::ExitCode;

use clap::Parser;
use firebreak_mcp::{HttpAgent, serve};
use tokio::io::{BufReader, stdin, stdout};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("firebreak-mcp: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let agent = HttpAgent::new(&cli.agent)?;
    eprintln!(
        "firebreak-mcp: serving MCP on standard input and output for the agent at {}",
        cli.agent
    );
    serve(BufReader::new(stdin()), stdout(), &agent)
        .await
        .map_err(|error| format!("standard input or output failed: {error}"))
}

/// The Firebreak payment agent as a Model Context Protocol server on standard input and output.
///
/// It offers an AI model two tools: `allowance_status`, and `pay`, which can pay only the
/// allowance's merchant. It holds no keys: it forwards to the agent's HTTP API and relays the
/// agent's answers.
#[derive(Debug, Parser)]
#[command(version)]
struct Cli {
    /// The payment agent's HTTP URL.
    #[arg(long, default_value = "http://127.0.0.1:7741")]
    agent: String,
}
