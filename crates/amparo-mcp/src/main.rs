//! `amparo-mcp-serve` — expose the default Amparo tool registry over MCP.
//!
//! Deny-by-default, explicitly: with no flags, every `tools/call` is
//! refused by the deny-all engine. `--policy-url` wires a remote engine
//! (Guardrail is a commercial implementation of the same wire protocol);
//! `--allow-all` is an explicit opt-in for local experiments. Human
//! approval stays auto-deny unless `--auto-approve` is passed — remote MCP
//! clients have no human at the terminal by default.
//!
//! This binary is a thin wrapper over [`amparo_mcp::serve`]; the same
//! implementation backs the `amparo mcp-serve` subcommand, so behavior,
//! help text and exit codes stay identical across both entry points.

use amparo_mcp::serve::{self, ParseResult};

#[tokio::main]
async fn main() {
    match serve::parse_flags(std::env::args().skip(1)) {
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
