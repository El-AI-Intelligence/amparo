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
//! - `amparo skill add|propose|list|show|adopt|check|retire [FLAGS]`
//!   manages skills (M6c/M6d): operator-authored or distilled candidate
//!   procedures adopted behind the same policy + approval gates as a tool
//!   call, re-checked and retired on policy drift or performance.
//! - `amparo notebook list|promote|rollup [FLAGS]` works the rollup and
//!   archival layer (M6e): list cold-archive records, pin one into the hot
//!   layer, or force the promote + fold on demand (cron-able).
//! - `amparo privacy [FLAGS]` reads the privacy ledger (M7): the summary
//!   plus the most recent network-call and PII-strip rows — the reviewer's
//!   front door to "who allowed this, and under what policy".
//! - `amparo doctor [FLAGS]` runs the operator's QA pass (M9): one
//!   deterministic, read-only sweep over the workspace, ledger, sessions,
//!   notebook, skills, schedule, policy-engine reachability and chat
//!   config — exit 0 healthy, 1 problems, 2 usage.
//! - `amparo schedule list|cancel [FLAGS]` inspects the schedule queue
//!   (M8): list every persisted promise or cancel a pending one — a status
//!   change, never a deletion; the chat driver's ticker does the firing.
//! - `amparo tui [FLAGS]` is the interactive terminal surface: one prompt,
//!   the whole gate chain rendered live — banner, gutter rows, approval
//!   cards, status line. Piped, it degrades to one task per stdin line with
//!   zero escapes.
//! - `amparo code [DIR]` is the coding-terminal surface (M13): an
//!   alternate-screen file tree with the workspace's git marks, file
//!   opening, and diff-accept editing — one instruction on the open
//!   file, the proposed change rendered as a diff and applied with a
//!   single `y` under a 60-second fail-closed deadline (the run/build
//!   pane is still ahead). Piped, it degrades to a plain-text report
//!   with zero escapes.
//! - `amparo wizard` writes the first-run profile: four ruled steps —
//!   workspace, LLM endpoint, optional Guardrail policy (wire check URL,
//!   key, console URL), optional Engram memory URL — saved locally (mode
//!   0600) and read back at boot to fill environment gaps (env always
//!   wins).
//! - `amparo version` prints the version.
//!
//! stdout carries the final answer only (a scripting contract); progress,
//! gate decisions, the report and errors go to stderr. `amparo tui` is the
//! exception: it renders its whole surface on stdout.

mod approve;
// The coding surface's interactive reader is Unix-only by design — on
// Windows the surface runs piped (the report), so the machinery
// compiles but is never constructed. Allow the dead code there instead
// of faking a Windows reader.
#[cfg_attr(not(unix), allow(dead_code, unused_variables))]
mod code;
mod doctor;
mod events;
mod notebook;
mod privacy;
mod raw;
mod run;
mod schedule;
mod skill;
mod stderr_subscriber;
// The interactive reader (raw mode, keypress approvals, picker) is
// Unix-only by design — on Windows the surface runs piped, so the
// machinery compiles but is never constructed. Allow the dead code there
// instead of faking a Windows reader.
#[cfg_attr(not(unix), allow(dead_code, unused_variables))]
mod tui;
mod wizard;

use amparo_mcp::serve::{self, ParseResult};

const USAGE: &str = "\
amparo — the policy-governed AI agent

USAGE:
  amparo run [FLAGS] \"task\"
  amparo mcp-serve [FLAGS]
  amparo chat telegram|discord|slack [FLAGS]
  amparo skill add|propose|list|show|adopt|check|retire [FLAGS]
  amparo notebook list|promote|rollup [FLAGS]
  amparo privacy [FLAGS]
  amparo doctor [FLAGS]
  amparo schedule list|cancel [FLAGS]
  amparo tui [FLAGS]
  amparo code [DIR]
  amparo wizard
  amparo version

SUBCOMMANDS:
  run        drive the agent loop end-to-end (stdout: final answer only)
  mcp-serve  expose the default tool registry over MCP (stdio JSON-RPC 2.0)
  chat       serve the agent over a messaging platform (inline-button approval)
  skill      manage skills: author candidates, propose from the notebook,
             list/show adopted, adopt behind policy + approval
  notebook   roll up and inspect the lab notebook: list records, promote a
             case into the hot layer, force the promote + fold
  privacy    read the privacy ledger: a summary plus the most recent
             network-call and PII-strip rows
  doctor     the operator's QA pass: workspace, ledger, sessions, notebook,
             skills, schedule, policy, engram, chat-config
  schedule   inspect the schedule queue: list every persisted promise or
             cancel a pending one (a status change, never a deletion)
  tui        the interactive terminal surface: one prompt, the whole gate
             chain rendered live (banner, gutter rows, approval cards,
             status line); piped: one task per stdin line
  code       the coding-terminal surface: an alternate-screen file tree
             with git marks and diff-accept editing; piped: a plain-text
             report
  wizard     the first-run profile: workspace, LLM endpoint, optional
             Guardrail policy (check URL, key, console URL), optional
             Engram memory URL — saved locally (0600), read back at boot
             to fill environment gaps
  version    print the version

Run `amparo run --help`, `amparo mcp-serve --help`, `amparo chat --help`,
`amparo skill --help`, `amparo notebook --help`, `amparo privacy --help`,
`amparo doctor --help`, `amparo schedule --help`, `amparo tui --help`,
`amparo code --help` or `amparo wizard --help` for flags.";

#[tokio::main]
async fn main() {
    // Agent warnings (a checkpoint save failure, a corrupt session file)
    // travel over tracing — install the minimal warn/error subscriber so
    // the operator sees them on stderr. If a host already installed one,
    // keep theirs.
    let _ = tracing::subscriber::set_global_default(stderr_subscriber::StderrWarnSubscriber);
    let mut args = std::env::args().skip(1);
    let Some(cmd) = args.next() else {
        println!("{USAGE}");
        std::process::exit(2);
    };
    match cmd.as_str() {
        "run" => run::dispatch(args).await,
        "mcp-serve" => mcp_serve(args).await,
        "chat" => chat(args).await,
        "skill" => skill::dispatch(args).await,
        "notebook" => notebook::dispatch(args),
        "privacy" => privacy::dispatch(args),
        "doctor" => doctor::dispatch(args).await,
        "schedule" => schedule::dispatch(args),
        "tui" => tui::dispatch(args).await,
        "code" => code::dispatch(args).await,
        "wizard" => wizard::dispatch(args).await,
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
