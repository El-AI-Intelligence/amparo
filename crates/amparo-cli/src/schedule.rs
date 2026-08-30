//! The `amparo schedule` subcommand — the operator's window on the
//! schedule queue (M8 W5).
//!
//! The queue lives at `<workspace>/.amparo/schedule/` and is written by
//! chat tasks whose profile opens the `schedule` tool; the chat driver's
//! ticker fires each due promise back through the full gate chain. The
//! CLI gets inspection, not a daemon: `list` shows every promise and
//! `cancel` moves a pending promise to cancelled. Cancel is a status
//! change, never a deletion — the record of what was promised survives.
//! Nothing here starts, fires or rewrites the queue beyond a cancel.
//!
//! Exit codes follow the `amparo run` contract: usage problems exit 2,
//! runtime failures (an unreadable queue, or a cancel of a missing or
//! non-pending promise) exit 1.

use amparo_chat::{schedule_dir, JsonScheduleStore, ScheduleStore, ScheduledStatus};
use amparo_tools::PathPolicy;

pub const SCHEDULE_USAGE: &str = "\
amparo schedule — inspect and cancel the schedule queue (M8 W5)

USAGE:
  amparo schedule list [--workspace DIR]
  amparo schedule cancel <id> [--workspace DIR]

The queue lives at <workspace>/.amparo/schedule/: chat tasks whose
profile opens the schedule tool persist promises there, and the chat
driver's ticker fires each due promise back through the full gate chain
— the same policy engine, human-approval gate, ledger and checkpoints a
live task gets. A promise whose instant passes beyond the grace window
is marked missed, never fired late. The CLI only inspects: nothing here
starts or fires a promise.

FLAGS:
  --workspace DIR    workspace root (sets AMPARO_WORKSPACE); the queue
                     lives under <workspace>/.amparo/schedule/";

// ─────────────────────────────────────────────── Parsing ─────────────────────

/// Outcome of parsing: print usage (exit 0), a usage error (exit 2), or
/// a command to run.
#[derive(Debug)]
enum ParsedSchedule {
    Help,
    Error(String),
    List {
        workspace: Option<String>,
    },
    Cancel {
        id: String,
        workspace: Option<String>,
    },
}

/// Parse `amparo schedule` arguments. Never panics and never exits.
fn parse(args: Vec<String>) -> ParsedSchedule {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return ParsedSchedule::Help;
    }
    let mut workspace = None;
    let mut positionals: Vec<String> = Vec::new();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--workspace" => match iter.next() {
                Some(dir) => workspace = Some(dir),
                None => return ParsedSchedule::Error("--workspace requires a directory".into()),
            },
            other if other.starts_with('-') => {
                return ParsedSchedule::Error(format!(
                    "unknown flag {other}; see `amparo schedule --help`"
                ))
            }
            other => positionals.push(other.to_string()),
        }
    }
    match positionals.as_slice() {
        [cmd, rest @ ..] if cmd == "list" => {
            if rest.is_empty() {
                ParsedSchedule::List { workspace }
            } else {
                ParsedSchedule::Error(format!(
                    "amparo schedule list takes no arguments, got '{}'",
                    rest[0]
                ))
            }
        }
        [cmd, id] if cmd == "cancel" => ParsedSchedule::Cancel {
            id: id.clone(),
            workspace,
        },
        [cmd] if cmd == "cancel" => {
            ParsedSchedule::Error("amparo schedule cancel requires an id".into())
        }
        [cmd, ..] if cmd == "cancel" => {
            ParsedSchedule::Error("amparo schedule cancel takes exactly one id".into())
        }
        [other, ..] => ParsedSchedule::Error(format!(
            "unknown schedule command '{other}' — expected list or cancel"
        )),
        [] => ParsedSchedule::Error("amparo schedule requires a command: list | cancel".into()),
    }
}

// ─────────────────────────────────────────────── Dispatch ────────────────────

/// Entry point for `amparo schedule` (exit codes: 0 ok/help, 2 usage,
/// 1 runtime). Fully synchronous — the queue is a local directory.
pub fn dispatch(args: impl Iterator<Item = String>) {
    match parse(args.collect()) {
        ParsedSchedule::Help => println!("{SCHEDULE_USAGE}"),
        ParsedSchedule::Error(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
        ParsedSchedule::List { workspace } => {
            if let Err(message) = list(workspace.as_deref()) {
                eprintln!("amparo schedule: {message}");
                std::process::exit(1);
            }
        }
        ParsedSchedule::Cancel { id, workspace } => {
            if let Err(message) = cancel(&id, workspace.as_deref()) {
                eprintln!("amparo schedule: {message}");
                std::process::exit(1);
            }
        }
    }
}

/// The queue store for one command: `--workspace` wins (the run.rs
/// pattern), then the environment's workspace root.
fn store_for(workspace_flag: Option<&str>) -> JsonScheduleStore {
    if let Some(dir) = workspace_flag {
        std::env::set_var("AMPARO_WORKSPACE", dir);
    }
    let workspace = PathPolicy::from_env().workspace_root;
    JsonScheduleStore::new(schedule_dir(&workspace))
}

/// Print every promise in the queue, one line each: id, instant,
/// status, tenant, task. Reading never creates the queue dir.
fn list(workspace_flag: Option<&str>) -> Result<(), String> {
    let store = store_for(workspace_flag);
    let tasks = store.load_all();
    if tasks.is_empty() {
        println!("no scheduled tasks in {}", store.dir().display());
        return Ok(());
    }
    for task in &tasks {
        println!(
            "{}  {}  {}  {}  {}",
            task.id, task.at, task.status, task.tenant, task.task
        );
    }
    Ok(())
}

/// Move one pending promise to cancelled. A missing id or a promise
/// that already left `pending` is a runtime failure (exit 1) — cancel
/// is for waiting promises only.
fn cancel(id: &str, workspace_flag: Option<&str>) -> Result<(), String> {
    let store = store_for(workspace_flag);
    let Some(mut task) = store
        .load(id)
        .map_err(|e| format!("cannot read the schedule queue: {e}"))?
    else {
        return Err(format!("no such schedule: {id}"));
    };
    if task.status != ScheduledStatus::Pending {
        return Err(format!(
            "schedule {id} is {} — only a pending promise can be cancelled",
            task.status
        ));
    }
    task.status = ScheduledStatus::Cancelled;
    task.result = Some("cancelled by the operator".to_string());
    store
        .save(&task)
        .map_err(|e| format!("cannot persist the cancellation: {e}"))?;
    println!("cancelled {id}");
    Ok(())
}

// ───────────────────────────────────────────────────────────── Tests ─────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_list(args: &[&str]) -> Option<String> {
        match parse(args.iter().map(|s| s.to_string()).collect()) {
            ParsedSchedule::List { workspace } => workspace,
            other => panic!("expected list, got {other:?}"),
        }
    }

    fn parse_cancel(args: &[&str]) -> (String, Option<String>) {
        match parse(args.iter().map(|s| s.to_string()).collect()) {
            ParsedSchedule::Cancel { id, workspace } => (id, workspace),
            other => panic!("expected cancel, got {other:?}"),
        }
    }

    fn parse_error(args: &[&str]) -> String {
        match parse(args.iter().map(|s| s.to_string()).collect()) {
            ParsedSchedule::Error(message) => message,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn help_is_recognized_anywhere() {
        assert!(matches!(parse(vec!["--help".into()]), ParsedSchedule::Help));
        assert!(matches!(parse(vec!["-h".into()]), ParsedSchedule::Help));
        assert!(matches!(
            parse(vec!["list".into(), "--help".into()]),
            ParsedSchedule::Help
        ));
    }

    #[test]
    fn parses_list_with_an_optional_workspace() {
        assert_eq!(parse_list(&["list"]), None);
        assert_eq!(
            parse_list(&["list", "--workspace", "/tmp/ws"]).as_deref(),
            Some("/tmp/ws")
        );
        assert_eq!(
            parse_list(&["--workspace", "/tmp/ws", "list"]).as_deref(),
            Some("/tmp/ws")
        );
    }

    #[test]
    fn parses_cancel_with_an_optional_workspace() {
        assert_eq!(parse_cancel(&["cancel", "sched-1"]).0, "sched-1");
        let (id, workspace) = parse_cancel(&["--workspace", "/tmp/ws", "cancel", "sched-1"]);
        assert_eq!(id, "sched-1");
        assert_eq!(workspace.as_deref(), Some("/tmp/ws"));
    }

    #[test]
    fn rejects_bad_usage() {
        assert!(
            parse_error(&[]).contains("requires a command"),
            "{}",
            parse_error(&[])
        );
        assert!(parse_error(&["nonsense"]).contains("unknown schedule command 'nonsense'"));
        assert!(parse_error(&["list", "extra"]).contains("takes no arguments"));
        assert!(parse_error(&["cancel"]).contains("requires an id"));
        assert!(parse_error(&["cancel", "a", "b"]).contains("exactly one id"));
        assert!(parse_error(&["--nonsense"]).contains("unknown flag --nonsense"));
        assert!(parse_error(&["list", "--workspace"]).contains("requires a directory"));
    }
}
