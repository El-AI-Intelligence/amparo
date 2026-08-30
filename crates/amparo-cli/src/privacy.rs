//! The `amparo privacy` subcommand — the reviewer's front door to the
//! privacy ledger (M7).
//!
//! The ledger is always-on in both hosts: every execution attempt of a
//! network-touching tool (`web_search`, `fetch_url`, `run_command`) writes
//! a row carrying the tool, the host at most (never a path, query or
//! command), the outcome, and whether a human approved or denied it —
//! and every PII strip writes per-category counts, never the values.
//! This surface answers the audit question "who allowed this, and under
//! what policy": one summary followed by the most recent rows, newest
//! first.
//!
//! Exit codes follow the `amparo run` contract: usage problems exit 2,
//! runtime failures (an unreadable ledger) exit 1.

use amparo_privacy::{
    privacy_dir, read_ledger, recorded_quota, LedgerKind, LedgerRow, LedgerSummary,
};
use amparo_tools::PathPolicy;

pub const PRIVACY_USAGE: &str = "\
amparo privacy — read the privacy ledger (M7)

USAGE:
  amparo privacy [--workspace DIR] [--tenant T] [--last N]

Reads the always-on privacy ledger at <workspace>/.amparo/privacy/
ledger.jsonl: one summary (file size and quota, network calls with human
gate answers, PII strips, rotations) followed by the last N rows, newest
first. Every amparo run and chat task writes the ledger; rows never
carry PII values, query strings or command text — a site is host-only.
Reading never creates, rewrites or rotates the ledger.

FLAGS:
  --workspace DIR    workspace root (sets AMPARO_WORKSPACE); the ledger
                     lives under <workspace>/.amparo/privacy/
  --tenant T         show rows for tenant T only (default \"cli\")
  --last N           at most N rows, newest first (default 10)";

/// The default `--last` tail length.
const DEFAULT_LAST: usize = 10;

// ─────────────────────────────────────────────── Parsing ─────────────────────

/// Parsed `amparo privacy` flags. `tenant` defaults to `\"cli\"` at use;
/// `last` defaults to [`DEFAULT_LAST`] at use.
#[derive(Debug)]
struct PrivacyFlags {
    workspace: Option<String>,
    tenant: Option<String>,
    last: Option<usize>,
}

impl Default for PrivacyFlags {
    fn default() -> Self {
        Self {
            workspace: None,
            tenant: None,
            last: None,
        }
    }
}

impl PrivacyFlags {
    fn tenant(&self) -> &str {
        self.tenant.as_deref().unwrap_or("cli")
    }
}

/// Outcome of parsing: print usage (exit 0), a usage error (exit 2), or
/// flags to execute.
#[derive(Debug)]
enum ParsedPrivacy {
    Help,
    Error(String),
    Run(PrivacyFlags),
}

/// Parse `amparo privacy` arguments. Never panics and never exits.
fn parse(args: Vec<String>) -> ParsedPrivacy {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return ParsedPrivacy::Help;
    }
    let mut flags = PrivacyFlags::default();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--workspace" => match iter.next() {
                Some(dir) => flags.workspace = Some(dir),
                None => return ParsedPrivacy::Error("--workspace requires a directory".into()),
            },
            "--tenant" => match iter.next() {
                Some(tenant) => flags.tenant = Some(tenant),
                None => return ParsedPrivacy::Error("--tenant requires a tenant id".into()),
            },
            "--last" => match iter.next() {
                Some(n) => match n.parse::<usize>() {
                    Ok(last) if last > 0 => flags.last = Some(last),
                    _ => {
                        return ParsedPrivacy::Error(format!(
                            "--last must be a positive integer, got '{n}'"
                        ))
                    }
                },
                None => return ParsedPrivacy::Error("--last requires a number".into()),
            },
            other if other.starts_with('-') => {
                return ParsedPrivacy::Error(format!(
                    "unknown flag {other}; see `amparo privacy --help`"
                ))
            }
            other => {
                return ParsedPrivacy::Error(format!(
                    "amparo privacy takes no positional arguments, got '{other}'"
                ))
            }
        }
    }
    ParsedPrivacy::Run(flags)
}

// ─────────────────────────────────────────────── Dispatch ────────────────────

/// Entry point for `amparo privacy` (exit codes: 0 ok/help, 2 usage,
/// 1 runtime). Fully synchronous — the ledger is a local file.
pub fn dispatch(args: impl Iterator<Item = String>) {
    match parse(args.collect()) {
        ParsedPrivacy::Help => println!("{PRIVACY_USAGE}"),
        ParsedPrivacy::Error(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
        ParsedPrivacy::Run(flags) => {
            if let Err(message) = execute(&flags) {
                eprintln!("amparo privacy: {message}");
                std::process::exit(1);
            }
        }
    }
}

/// Run one parsed command. `Err` is a runtime failure (exit 1).
fn execute(flags: &PrivacyFlags) -> Result<(), String> {
    // Apply --workspace (process-wide, the run.rs pattern) and resolve
    // the root the same way the tools do.
    if let Some(dir) = &flags.workspace {
        std::env::set_var("AMPARO_WORKSPACE", dir);
    }
    let workspace = PathPolicy::from_env().workspace_root;
    let path = privacy_dir(&workspace).join("ledger.jsonl");
    // A read-only surface: never create the ledger just by reading it.
    if !path.exists() {
        eprintln!(
            "[privacy] no ledger for tenant {} (runs write rows at {})",
            flags.tenant(),
            path.display()
        );
        return Ok(());
    }
    // Read-only: the free functions never open a store, so the ledger is
    // not created, rewritten or rotated just by being read — and the
    // quota sidecar is read before anything could touch it.
    let quota = recorded_quota(&path);
    let mut rows =
        read_ledger(&path).map_err(|e| format!("cannot read the privacy ledger: {e}"))?;
    rows.retain(|row| row.tenant == flags.tenant());

    let summary = LedgerSummary::compute(&rows);
    println!("tenant {}", flags.tenant());
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    match quota {
        Some(quota) => println!("ledger: {bytes} bytes (quota {})", quota.max_bytes),
        None => println!("ledger: {bytes} bytes (unbounded)"),
    }
    if summary.rotations > 0 {
        println!(
            "rotations: {} (rows dropped {})",
            summary.rotations, summary.rows_dropped
        );
    }
    println!(
        "network calls: {} (approved {}, denied {})",
        summary.network_calls, summary.human_approved, summary.human_denied
    );
    println!("pii strips: {}", summary.pii_strips);
    if !summary.by_tool.is_empty() {
        let tools = summary
            .by_tool
            .iter()
            .map(|(tool, count)| format!("{tool} x{count}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!("tools: {tools}");
    }
    if !summary.pii_by_category.is_empty() {
        let categories = summary
            .pii_by_category
            .iter()
            .map(|(category, count)| format!("{category} x{count}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!("categories: {categories}");
    }
    if !rows.is_empty() {
        println!();
        for row in rows.iter().rev().take(flags.last.unwrap_or(DEFAULT_LAST)) {
            println!("{}", render_row(row));
        }
    }
    Ok(())
}

/// One ledger row, one line — host and gate provenance, never values.
fn render_row(row: &LedgerRow) -> String {
    match row.kind {
        LedgerKind::NetworkCall => {
            let tool = row.tool.as_deref().unwrap_or("-");
            let site = row.site.as_deref().unwrap_or("-");
            let outcome = row.outcome.as_deref().unwrap_or("-");
            let gate = row.gate.as_deref().unwrap_or("-");
            format!(
                "{}  network  {}  {}  {}  {}",
                row.ts, tool, site, outcome, gate
            )
        }
        LedgerKind::PiiStrip => {
            let counts = if row.pii_counts.is_empty() {
                "-".to_string()
            } else {
                row.pii_counts
                    .iter()
                    .map(|(category, count)| format!("{category} x{count}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            format!("{}  pii-strip  {}", row.ts, counts)
        }
        LedgerKind::Rotated => {
            format!(
                "{}  rotated  dropped {} rows",
                row.ts,
                row.dropped_rows.unwrap_or(0)
            )
        }
    }
}

// ───────────────────────────────────────────────────────────── Tests ─────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(args: &[&str]) -> PrivacyFlags {
        match parse(args.iter().map(|s| s.to_string()).collect()) {
            ParsedPrivacy::Run(flags) => flags,
            ParsedPrivacy::Help => panic!("expected flags, got help"),
            ParsedPrivacy::Error(message) => panic!("expected flags, got: {message}"),
        }
    }

    fn parse_error(args: &[&str]) -> String {
        match parse(args.iter().map(|s| s.to_string()).collect()) {
            ParsedPrivacy::Error(message) => message,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn help_is_recognized_anywhere() {
        assert!(matches!(parse(vec!["--help".into()]), ParsedPrivacy::Help));
        assert!(matches!(parse(vec!["-h".into()]), ParsedPrivacy::Help));
        assert!(matches!(
            parse(vec!["--tenant".into(), "cli".into(), "--help".into()]),
            ParsedPrivacy::Help
        ));
    }

    #[test]
    fn parses_flags_with_defaults_for_the_rest() {
        let flags = parse_ok(&[]);
        assert_eq!(flags.tenant(), "cli");
        assert!(flags.last.is_none());
        assert!(flags.workspace.is_none());

        let flags = parse_ok(&[
            "--workspace",
            "/tmp/ws",
            "--tenant",
            "telegram:1",
            "--last",
            "7",
        ]);
        assert_eq!(flags.workspace.as_deref(), Some("/tmp/ws"));
        assert_eq!(flags.tenant(), "telegram:1");
        assert_eq!(flags.last, Some(7));
    }

    #[test]
    fn rejects_unknown_flags_missing_values_and_bad_numbers() {
        assert!(parse_error(&["--nonsense"]).contains("unknown flag --nonsense"));
        assert!(parse_error(&["--workspace"]).contains("requires a directory"));
        assert!(parse_error(&["--tenant"]).contains("requires a tenant id"));
        assert!(parse_error(&["--last"]).contains("requires a number"));
        assert!(parse_error(&["--last", "zero"]).contains("positive integer"));
        assert!(parse_error(&["--last", "0"]).contains("positive integer"));
    }

    #[test]
    fn rejects_positional_arguments() {
        assert!(parse_error(&["list"]).contains("takes no positional arguments, got 'list'"));
    }
}
