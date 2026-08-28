//! `amparo` — the policy-governed AI agent, one binary.
//!
//! - `amparo run "task"` drives the agent loop end-to-end against a BYO-LLM
//!   endpoint (fail-closed on `AMPARO_INFERENCE_URL`/`AMPARO_INFERENCE_MODEL`),
//!   behind the deny-by-default gate chain with interactive approval at the
//!   terminal.
//! - `amparo mcp-serve` exposes the default tool registry over MCP — the same
//!   implementation as the standalone `amparo-mcp-serve` binary, with the same
//!   help text, error strings and exit codes.
//! - `amparo version` prints the version.
//!
//! stdout carries the final answer only (a scripting contract); progress,
//! gate decisions, the report and errors go to stderr.

mod approve;
mod events;
mod run;

use amparo_mcp::serve::{self, ParseResult};

const USAGE: &str = "\
amparo — the policy-governed AI agent

USAGE:
  amparo run [FLAGS] \"task\"
  amparo mcp-serve [FLAGS]
  amparo version

SUBCOMMANDS:
  run        drive the agent loop end-to-end (stdout: final answer only)
  mcp-serve  expose the default tool registry over MCP (stdio JSON-RPC 2.0)
  version    print the version

Run `amparo run --help` or `amparo mcp-serve --help` for flags.";

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let Some(cmd) = args.next() else {
        println!("{USAGE}");
        std::process::exit(2);
    };
    match cmd.as_str() {
        "run" => run::dispatch(args).await,
        "mcp-serve" => mcp_serve(args).await,
        "version" | "-V" | "--version" => println!("amparo {}", env!("CARGO_PKG_VERSION")),
        "--help" | "-h" => println!("{USAGE}"),
        other => {
            eprintln!("unknown subcommand {other}; see `amparo --help`");
            std::process::exit(2);
        }
    }
}

/// The `mcp-serve` subcommand — byte-identical behavior to the standalone
/// `amparo-mcp-serve` binary (same parse, same help, same exit codes).
async fn mcp_serve(args: impl Iterator<Item = String>) {
    match serve::parse_flags(args) {
        ParseResult::Help => println!("{}", serve::HELP),
        ParseResult::Error(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
        ParseResult::Serve(flags) => {
            if let Err(e) = serve::run(flags).await {
                eprintln!("{}", e.message);
                std::process::exit(e.exit_code);
            }
        }
    }
}
