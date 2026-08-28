//! `amparo` — the policy-governed AI agent, one binary.
//!
//! - `amparo run "task"` drives the agent loop end-to-end against a BYO-LLM
//!   endpoint (fail-closed on `AMPARO_INFERENCE_URL`/`AMPARO_INFERENCE_MODEL`),
//!   behind the deny-by-default gate chain with interactive approval at the
//!   terminal.
//! - `amparo mcp-serve` exposes the default tool registry over MCP — the same
//!   implementation as the standalone `amparo-mcp-serve` binary, with the same
//!   help text, error strings and exit codes.
//! - `amparo chat telegram|discord|slack [FLAGS]` serves the agent over a
//!   messaging platform — long-polled Telegram with inline-button approval,
//!   Discord gateway and Slack Socket Mode, all behind the same deny-by-
//!   default gate chain as `run`.
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
  amparo chat telegram|discord|slack [FLAGS]
  amparo version

SUBCOMMANDS:
  run        drive the agent loop end-to-end (stdout: final answer only)
  mcp-serve  expose the default tool registry over MCP (stdio JSON-RPC 2.0)
  chat       serve the agent over a messaging platform (inline-button approval)
  version    print the version

Run `amparo run --help`, `amparo mcp-serve --help` or `amparo chat --help`
for flags.";

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
        "chat" => chat(args).await,
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

/// The `amparo chat` subcommand — parse the platform and flags, then serve
/// the adapter until Ctrl-C or a fatal failure. Exit codes match the
/// `amparo run` contract: 0 help, 2 flag/token problem, 1 serve failure.
async fn chat(args: impl Iterator<Item = String>) {
    amparo_chat::dispatch::dispatch(args).await;
}
