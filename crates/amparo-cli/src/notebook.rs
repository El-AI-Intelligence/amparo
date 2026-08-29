//! The `amparo notebook` subcommand — operator-side rollup and archival
//! (M6e).
//!
//! The cold archive (`records.jsonl`) is the record of every `--growth`
//! task and is never touched from here. This surface works the derived
//! **hot layer** — the informative subset (dedupe survivors plus gate
//! events of interest) that case retrieval reads:
//!
//! - `list` — read-only summaries of the cold archive, newest first, with
//!   the promotion flag.
//! - `promote` — pin one cold record into the hot layer (the operator
//!   review path from spec §3.2); promoted rows are fold-exempt.
//! - `rollup` — promote the cold tail and fold the hot layer on demand
//!   with explicit levers, cron-able (exit 0 even when rows fold);
//!   `--dry-run` reports the same numbers and writes nothing.
//!
//! Task starts promote and fold automatically (`--growth`); these commands
//! are the operator's levers on the same files. Exit codes follow the
//! `amparo run` contract: usage problems exit 2, runtime failures (unknown
//! record id, held lock) exit 1.

use amparo_notebook::{
    list_records, notebook_dir, promote_record, rollup, rollup_dry_run, PromoteOutcome,
    DEFAULT_MAX_BYTES, DEFAULT_ROLLUP_DAYS, MAX_BYTES_FLOOR,
};
use amparo_tools::PathPolicy;
use std::path::PathBuf;

pub const NOTEBOOK_USAGE: &str = "\
amparo notebook — roll up and inspect the lab notebook (M6e)

USAGE:
  amparo notebook list [--workspace DIR] [--tenant T] [--limit N]
  amparo notebook promote <record-id> [--workspace DIR] [--max-bytes B]
  amparo notebook rollup [--workspace DIR] [--days N] [--max-bytes B]
             [--dry-run]

COMMANDS:
  list      print cold-archive records for the tenant, newest first:
            <id>  <started_at> <status>/<verification>  <tool-sequence
            hash>  <task text> — plus `  [promoted]` on operator-promoted
            rows. Read-only.
  promote   pin one cold record into the hot layer — the operator review
            path. The hot copy keeps the cold id and created_at, and the
            row is exempt from the 90-day fold. Promoting an already-
            promoted id is a no-op (exit 0). Refuses unknown ids (exit 1).
  rollup    promote the cold tail and fold the hot layer with explicit
            levers (cron-able — exit 0 even when rows fold). The cold
            archive is never modified. --dry-run reports the same numbers
            and writes nothing.

FLAGS:
  --workspace DIR         workspace root (sets AMPARO_WORKSPACE); the
                          notebook lives under <workspace>/.amparo/notebook/
  --tenant T              list: tenant id (default \"cli\")
  --limit N               list: at most N rows (default 20)
  --days N                rollup: hot rows older than N days fold into
                          the cold archive (default 90)
  --max-bytes B           promote/rollup: payload cap per hot record in
                          bytes (default 4096, floor 1024)
  --dry-run               rollup: report the promoted/folded/kept counts
                          without writing anything";

// ─────────────────────────────────────────────── Parsing ─────────────────────

/// The parsed `amparo notebook` command.
#[derive(Debug)]
enum NotebookCommand {
    List(NotebookFlags),
    Promote { id: String, flags: NotebookFlags },
    Rollup(NotebookFlags),
}

/// Outcome of parsing: print usage (exit 0), a usage error (exit 2), or a
/// command to execute.
#[derive(Debug)]
enum ParsedNotebook {
    Help,
    Error(String),
    Run(NotebookCommand),
}

/// Parsed `amparo notebook` flags. `tenant` defaults to `\"cli\"` at use;
/// `limit` defaults to 20 at use; `days`/`max_bytes` default to the
/// crate's [`DEFAULT_ROLLUP_DAYS`]/[`DEFAULT_MAX_BYTES`] at use.
#[derive(Debug)]
struct NotebookFlags {
    workspace: Option<String>,
    tenant: Option<String>,
    limit: Option<usize>,
    days: Option<u64>,
    max_bytes: Option<usize>,
    dry_run: bool,
}

impl Default for NotebookFlags {
    fn default() -> Self {
        Self {
            workspace: None,
            tenant: None,
            limit: None,
            days: None,
            max_bytes: None,
            dry_run: false,
        }
    }
}

impl NotebookFlags {
    fn tenant(&self) -> &str {
        self.tenant.as_deref().unwrap_or("cli")
    }
}

/// Which subcommand-only flags [`parse_flags`] accepts.
#[derive(Clone, Copy, PartialEq)]
enum NotebookFlagExtras {
    /// No subcommand-only flags beyond `--max-bytes` (promote).
    None,
    /// `--tenant`, `--limit`.
    List,
    /// `--days`, `--max-bytes`, `--dry-run`.
    Rollup,
}

/// Parse the flags the notebook subcommands share (`extras` enables the
/// subcommand-only flags). `--help`/`-h` are handled before this runs.
fn parse_flags(
    args: Vec<String>,
    extras: NotebookFlagExtras,
) -> Result<(NotebookFlags, Vec<String>), String> {
    let mut flags = NotebookFlags::default();
    let mut positional = Vec::new();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--workspace" => match iter.next() {
                Some(dir) => flags.workspace = Some(dir),
                None => return Err("--workspace requires a directory".into()),
            },
            "--tenant" if extras == NotebookFlagExtras::List => match iter.next() {
                Some(tenant) => flags.tenant = Some(tenant),
                None => return Err("--tenant requires a tenant id".into()),
            },
            "--limit" if extras == NotebookFlagExtras::List => match iter.next() {
                Some(n) => match n.parse::<usize>() {
                    Ok(limit) if limit > 0 => flags.limit = Some(limit),
                    _ => {
                        return Err(format!(
                            "--limit must be a positive integer, got '{n}'"
                        ))
                    }
                },
                None => return Err("--limit requires a number".into()),
            },
            "--days" if extras == NotebookFlagExtras::Rollup => match iter.next() {
                Some(n) => match n.parse::<u64>() {
                    Ok(days) => flags.days = Some(days),
                    _ => {
                        return Err(format!(
                            "--days must be a non-negative integer, got '{n}'"
                        ))
                    }
                },
                None => return Err("--days requires a number".into()),
            },
            "--max-bytes"
                if extras == NotebookFlagExtras::None
                    || extras == NotebookFlagExtras::Rollup =>
            {
                match iter.next() {
                    Some(n) => match n.parse::<usize>() {
                        Ok(bytes) if bytes > 0 => flags.max_bytes = Some(bytes),
                        _ => {
                            return Err(format!(
                                "--max-bytes must be a positive integer, got '{n}'"
                            ))
                        }
                    },
                    None => return Err("--max-bytes requires a number".into()),
                }
            }
            "--dry-run" if extras == NotebookFlagExtras::Rollup => flags.dry_run = true,
            other if other.starts_with('-') => {
                return Err(format!(
                    "unknown flag {other}; see `amparo notebook --help`"
                ))
            }
            other => positional.push(other.to_string()),
        }
    }
    Ok((flags, positional))
}

/// Parse `amparo notebook` arguments. Never panics and never exits.
fn parse(args: Vec<String>) -> ParsedNotebook {
    if args.is_empty() {
        return ParsedNotebook::Error("missing subcommand; see `amparo notebook --help`".into());
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return ParsedNotebook::Help;
    }
    let mut iter = args.into_iter();
    let command = iter.next().expect("checked non-empty");
    let rest: Vec<String> = iter.collect();
    let result = match command.as_str() {
        "list" => parse_flags(rest, NotebookFlagExtras::List).and_then(|(flags, positional)| {
            if positional.is_empty() {
                Ok(NotebookCommand::List(flags))
            } else {
                Err("amparo notebook list takes no positional arguments".into())
            }
        }),
        "promote" => {
            parse_flags(rest, NotebookFlagExtras::None).and_then(|(flags, positional)| {
                match positional.len() {
                    1 => Ok(NotebookCommand::Promote {
                        id: positional.into_iter().next().expect("one positional"),
                        flags,
                    }),
                    _ => Err("amparo notebook promote requires a record id".into()),
                }
            })
        }
        "rollup" => parse_flags(rest, NotebookFlagExtras::Rollup).and_then(|(flags, positional)| {
            if positional.is_empty() {
                Ok(NotebookCommand::Rollup(flags))
            } else {
                Err("amparo notebook rollup takes no positional arguments".into())
            }
        }),
        other => Err(format!(
            "unknown notebook subcommand {other}; see `amparo notebook --help`"
        )),
    };
    match result {
        Ok(command) => ParsedNotebook::Run(command),
        Err(message) => ParsedNotebook::Error(message),
    }
}

// ─────────────────────────────────────────────── Dispatch ────────────────────

/// Entry point for `amparo notebook` (exit codes: 0 ok/help, 2 usage,
/// 1 runtime). Fully synchronous — the notebook is local files.
pub fn dispatch(args: impl Iterator<Item = String>) {
    match parse(args.collect()) {
        ParsedNotebook::Help => println!("{NOTEBOOK_USAGE}"),
        ParsedNotebook::Error(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
        ParsedNotebook::Run(command) => {
            if let Err(message) = execute(command) {
                eprintln!("amparo notebook: {message}");
                std::process::exit(1);
            }
        }
    }
}

/// Run one parsed command. `Err` is a runtime failure (exit 1).
fn execute(command: NotebookCommand) -> Result<(), String> {
    match command {
        NotebookCommand::List(flags) => list(&flags),
        NotebookCommand::Promote { id, flags } => promote(&id, &flags),
        NotebookCommand::Rollup(flags) => rollup_cmd(&flags),
    }
}

/// Apply `--workspace` (process-wide, the run.rs pattern) and return the
/// workspace root.
fn setup_workspace(flags: &NotebookFlags) -> PathBuf {
    if let Some(dir) = &flags.workspace {
        std::env::set_var("AMPARO_WORKSPACE", dir);
    }
    PathPolicy::from_env().workspace_root
}

// ─────────────────────────────────────────────── Commands ────────────────────

/// `notebook list` — read-only cold-archive summaries, newest first, with
/// the promotion flag.
fn list(flags: &NotebookFlags) -> Result<(), String> {
    let workspace = setup_workspace(flags);
    let tenant = flags.tenant();
    let rows = list_records(
        &notebook_dir(&workspace),
        tenant,
        flags.limit.unwrap_or(20),
    )?;
    if rows.is_empty() {
        eprintln!(
            "[notebook] no records for tenant {tenant} \
             (amparo run --growth writes records)"
        );
        return Ok(());
    }
    for row in &rows {
        let verdict = row.verification.as_deref().unwrap_or("-");
        let mut line = format!(
            "{}  {} {}/{}  {}  {}",
            row.id,
            row.started_at,
            row.status,
            verdict,
            row.tool_sequence_hash,
            row.task_text
        );
        if row.promoted {
            line.push_str("  [promoted]");
        }
        println!("{line}");
    }
    Ok(())
}

/// `notebook promote <record-id>` — pin one cold record into the hot
/// layer (spec §3.2). The hot copy keeps the cold id and created_at and
/// is fold-exempt; idempotent, refuses unknown ids.
fn promote(id: &str, flags: &NotebookFlags) -> Result<(), String> {
    let workspace = setup_workspace(flags);
    let max_bytes = flags
        .max_bytes
        .unwrap_or(DEFAULT_MAX_BYTES)
        .max(MAX_BYTES_FLOOR);
    match promote_record(
        &notebook_dir(&workspace),
        id,
        max_bytes,
        chrono::Utc::now(),
    )? {
        PromoteOutcome::Promoted => {
            eprintln!("[notebook] promoted {id} to the hot layer");
        }
        PromoteOutcome::AlreadyPromoted => {
            eprintln!("[notebook] {id} is already promoted to the hot layer");
        }
    }
    Ok(())
}

/// `notebook rollup` — the forced promote + fold (cron-able, exit 0).
/// `--dry-run` reports the same counts without writing anything.
fn rollup_cmd(flags: &NotebookFlags) -> Result<(), String> {
    let workspace = setup_workspace(flags);
    let nb_dir = notebook_dir(&workspace);
    let days = flags.days.unwrap_or(DEFAULT_ROLLUP_DAYS);
    let max_bytes = flags
        .max_bytes
        .unwrap_or(DEFAULT_MAX_BYTES)
        .max(MAX_BYTES_FLOOR);
    let now = chrono::Utc::now();
    let report = if flags.dry_run {
        rollup_dry_run(&nb_dir, days, max_bytes, now)?
    } else {
        rollup(&nb_dir, days, max_bytes, now)?
    };
    eprintln!(
        "[notebook] rollup: promoted {}, folded {}, kept {} record(s) in the hot layer{}",
        report.promoted,
        report.folded,
        report.kept,
        if flags.dry_run {
            " (dry-run — nothing written)"
        } else {
            ""
        }
    );
    Ok(())
}

// ───────────────────────────────────────────────────────────── Tests ─────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(args: &[&str]) -> NotebookCommand {
        match parse(args.iter().map(|s| s.to_string()).collect()) {
            ParsedNotebook::Run(command) => command,
            ParsedNotebook::Help => panic!("expected a command, got help"),
            ParsedNotebook::Error(message) => panic!("expected a command, got: {message}"),
        }
    }

    fn parse_error(args: &[&str]) -> String {
        match parse(args.iter().map(|s| s.to_string()).collect()) {
            ParsedNotebook::Error(message) => message,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn help_is_recognized_anywhere() {
        assert!(matches!(parse(vec!["--help".into()]), ParsedNotebook::Help));
        assert!(matches!(
            parse(vec!["list".into(), "-h".into()]),
            ParsedNotebook::Help
        ));
        assert!(parse_error(&[]).contains("missing subcommand"));
    }

    #[test]
    fn unknown_subcommand_and_flag_are_usage_errors() {
        assert!(parse_error(&["frobnicate"]).contains("unknown notebook subcommand"));
        assert!(parse_error(&["list", "--nonsense"]).contains("unknown flag --nonsense"));
        assert!(parse_error(&["list", "--days", "5"]).contains("unknown flag --days"));
        assert!(parse_error(&["rollup", "--limit", "5"]).contains("unknown flag --limit"));
    }

    #[test]
    fn list_parses_tenant_limit_and_defaults() {
        match parse_ok(&["list"]) {
            NotebookCommand::List(flags) => {
                assert_eq!(flags.tenant(), "cli");
                assert!(flags.limit.is_none());
            }
            other => panic!("expected List, got {other:?}"),
        }
        match parse_ok(&["list", "--tenant", "telegram:1", "--limit", "7"]) {
            NotebookCommand::List(flags) => {
                assert_eq!(flags.tenant(), "telegram:1");
                assert_eq!(flags.limit, Some(7));
            }
            other => panic!("expected List, got {other:?}"),
        }
        assert!(parse_error(&["list", "--limit", "0"]).contains("positive integer"));
        assert!(parse_error(&["list", "--limit"]).contains("requires a number"));
        assert!(parse_error(&["list", "x"]).contains("no positional"));
    }

    #[test]
    fn promote_requires_exactly_one_id() {
        match parse_ok(&["promote", "rec-1", "--max-bytes", "2048"]) {
            NotebookCommand::Promote { id, flags } => {
                assert_eq!(id, "rec-1");
                assert_eq!(flags.max_bytes, Some(2048));
            }
            other => panic!("expected Promote, got {other:?}"),
        }
        assert!(parse_error(&["promote"]).contains("requires a record id"));
        assert!(parse_error(&["promote", "a", "b"]).contains("requires a record id"));
        assert!(parse_error(&["promote", "rec-1", "--max-bytes", "0"])
            .contains("positive integer"));
        // The rollup-only flags are rejected for promote.
        assert!(parse_error(&["promote", "rec-1", "--dry-run"]).contains("unknown flag --dry-run"));
    }

    #[test]
    fn rollup_parses_days_max_bytes_and_dry_run() {
        match parse_ok(&["rollup"]) {
            NotebookCommand::Rollup(flags) => {
                assert!(flags.days.is_none());
                assert!(flags.max_bytes.is_none());
                assert!(!flags.dry_run);
            }
            other => panic!("expected Rollup, got {other:?}"),
        }
        match parse_ok(&["rollup", "--days", "0", "--max-bytes", "1024", "--dry-run"]) {
            NotebookCommand::Rollup(flags) => {
                assert_eq!(flags.days, Some(0));
                assert_eq!(flags.max_bytes, Some(1024));
                assert!(flags.dry_run);
            }
            other => panic!("expected Rollup, got {other:?}"),
        }
        assert!(parse_error(&["rollup", "--days", "-1"]).contains("non-negative"));
        assert!(parse_error(&["rollup", "--days"]).contains("requires a number"));
        assert!(parse_error(&["rollup", "x"]).contains("no positional"));
        // The list-only flag is rejected for rollup.
        assert!(parse_error(&["rollup", "--tenant", "cli"]).contains("unknown flag --tenant"));
    }
}
