//! The `amparo doctor` subcommand — the operator's QA pass (M9 W2).
//!
//! One deterministic, read-only sweep over everything Amparo writes into a
//! workspace, plus the configured surfaces: workspace existence and
//! writability, the privacy ledger, session checkpoints (stale `Running`
//! files older than 7 days), the notebook/skills/schedule JSONL files
//! (parseability, never contents), policy-engine reachability (a TCP dial,
//! or one real `/check` probe with `--probe`), and the chat TOML config.
//!
//! Reading never creates, rewrites or rotates anything — the only write is
//! the workspace writability probe, which creates and removes one
//! `.amparo-doctor-probe` file at the workspace root. `--probe` is the one
//! deliberate side effect: it consumes one engine check.
//!
//! Exit codes follow the `amparo run` contract: usage problems exit 2,
//! a found problem (an unreadable ledger, a stale checkpoint, an
//! unreachable engine) exits 1, a clean sweep exits 0 — cron-able.

use amparo_agent::{Checkpoint, SessionStatus};
use amparo_chat::{schedule_dir, ChatConfig};
use amparo_notebook::{
    notebook_dir, skills_dir, ADOPTED_FILE, HOT_FILE, HOT_HASHES_FILE, PROMOTED_FILE,
    PROPOSALS_FILE, RECHECKS_FILE, ROLLUP_STATE_FILE,
};
use amparo_policy::wire::WirePolicyEngine;
use amparo_policy::{PolicyEngine, PolicyVerdict};
use amparo_privacy::privacy_dir;
use amparo_tools::PathPolicy;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DOCTOR_USAGE: &str = "\
amparo doctor — the operator's QA pass (M9)

USAGE:
  amparo doctor [--workspace DIR] [--policy-url URL] [--probe]
                [--chat-config PATH]

Runs deterministic, read-only checks against a workspace and the
configured surfaces, printing one [doctor] line per check and one
[doctor] problem line per finding (problems go to stderr). A missing
file is information, not a problem — a fresh workspace has no ledger,
no sessions, no notebook. An unreadable or inconsistent file is a
problem. Exit codes: 0 healthy / 1 problems / 2 usage.

CHECKS:
  workspace     exists, is a directory, and accepts a probe file
  ledger        ledger.jsonl readable; row count
  sessions      checkpoints parse; Running older than 7 days is stale
  notebook      records/hot/hot-hashes/promoted JSONL + rollup.json parse
  skills        adopted/proposals/rechecks JSONL parse
  schedule      queue files parse; status counts
  policy        TCP dial to the engine; --probe sends one real /check
  chat-config   --chat-config TOML parses

FLAGS:
  --workspace DIR    workspace root (sets AMPARO_WORKSPACE)
  --policy-url URL   policy engine to reachability-check
  --probe            one real /check round trip (consumes one engine
                     check); requires --policy-url
  --chat-config PATH TOML chat config to parse-check";

/// A `Running` checkpoint older than this is reported as stale.
const STALE_RUNNING_SECS: u64 = 7 * 24 * 60 * 60;

/// The cold-archive file name — pinned by the M6a store (`JsonlStore`
/// writes it; no public const exists).
const RECORDS_FILE: &str = "records.jsonl";

// ─────────────────────────────────────────────── Parsing ─────────────────────

/// Parsed `amparo doctor` flags.
#[derive(Debug)]
struct DoctorFlags {
    workspace: Option<String>,
    policy_url: Option<String>,
    probe: bool,
    chat_config: Option<String>,
}

impl Default for DoctorFlags {
    fn default() -> Self {
        Self {
            workspace: None,
            policy_url: None,
            probe: false,
            chat_config: None,
        }
    }
}

/// Outcome of parsing: print usage (exit 0), a usage error (exit 2), or
/// flags to execute.
#[derive(Debug)]
enum ParsedDoctor {
    Help,
    Error(String),
    Run(DoctorFlags),
}

/// Parse `amparo doctor` arguments. Never panics and never exits.
fn parse(args: Vec<String>) -> ParsedDoctor {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return ParsedDoctor::Help;
    }
    let mut flags = DoctorFlags::default();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--workspace" => match iter.next() {
                Some(dir) => flags.workspace = Some(dir),
                None => return ParsedDoctor::Error("--workspace requires a directory".into()),
            },
            "--policy-url" => match iter.next() {
                Some(url) => flags.policy_url = Some(url),
                None => return ParsedDoctor::Error("--policy-url requires a URL".into()),
            },
            "--probe" => flags.probe = true,
            "--chat-config" => match iter.next() {
                Some(path) => flags.chat_config = Some(path),
                None => return ParsedDoctor::Error("--chat-config requires a path".into()),
            },
            other if other.starts_with('-') => {
                return ParsedDoctor::Error(format!(
                    "unknown flag {other}; see `amparo doctor --help`"
                ))
            }
            other => {
                return ParsedDoctor::Error(format!(
                    "amparo doctor takes no positional arguments, got '{other}'"
                ))
            }
        }
    }
    // A probe without an engine to probe is a usage error, not a finding.
    if flags.probe && flags.policy_url.is_none() {
        return ParsedDoctor::Error("--probe requires --policy-url".into());
    }
    ParsedDoctor::Run(flags)
}

// ─────────────────────────────────────────────── Dispatch ────────────────────

/// Entry point for `amparo doctor` (exit codes: 0 healthy/help, 2 usage,
/// 1 problems). The probe is the one async leg — a real engine check.
pub async fn dispatch(args: impl Iterator<Item = String>) {
    match parse(args.collect()) {
        ParsedDoctor::Help => println!("{DOCTOR_USAGE}"),
        ParsedDoctor::Error(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
        ParsedDoctor::Run(flags) => {
            if !execute(&flags).await {
                std::process::exit(1);
            }
        }
    }
}

/// Run the sweep. `true` = healthy (exit 0); `false` = problems (exit 1).
async fn execute(flags: &DoctorFlags) -> bool {
    // Apply --workspace (process-wide, the run.rs pattern) and resolve
    // the root the same way the tools do.
    if let Some(dir) = &flags.workspace {
        std::env::set_var("AMPARO_WORKSPACE", dir);
    }
    let workspace = PathPolicy::from_env().workspace_root;
    run_checks(&workspace, flags).await
}

// ─────────────────────────────────────────────── The checks ──────────────────

/// The sweep itself, split from [`execute`] so tests can point it at an
/// explicit root without touching process-wide env vars. Returns whether
/// no problems were found.
async fn run_checks(workspace: &Path, flags: &DoctorFlags) -> bool {
    let mut problems: Vec<String> = Vec::new();

    check_workspace(workspace, &mut problems);
    check_ledger(workspace, &mut problems);
    check_sessions(workspace, &mut problems);
    check_jsonl_dir(
        &notebook_dir(workspace),
        "notebook",
        &[
            ("records", RECORDS_FILE),
            ("hot", HOT_FILE),
            ("hot-hashes", HOT_HASHES_FILE),
            ("promoted", PROMOTED_FILE),
        ],
        &mut problems,
    );
    check_jsonl_dir(
        &skills_dir(workspace),
        "skills",
        &[
            ("adopted", ADOPTED_FILE),
            ("proposals", PROPOSALS_FILE),
            ("rechecks", RECHECKS_FILE),
        ],
        &mut problems,
    );
    // The rollup state is one JSON document, not JSONL — a separate check.
    let rollup = notebook_dir(workspace).join(ROLLUP_STATE_FILE);
    if rollup.exists() {
        match std::fs::read_to_string(&rollup)
            .map_err(|e| e.to_string())
            .and_then(|text| {
                serde_json::from_str::<serde_json::Value>(&text)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }) {
            Ok(()) => {}
            Err(e) => problems.push(format!("notebook {} unreadable: {e}", rollup.display())),
        }
    }
    check_schedule(workspace, &mut problems);
    check_policy(flags, &mut problems).await;
    check_chat_config(flags, &mut problems);

    for problem in &problems {
        eprintln!("[doctor] problem: {problem}");
    }
    if problems.is_empty() {
        println!("[doctor] healthy");
        true
    } else {
        println!("[doctor] {} problem(s)", problems.len());
        false
    }
}

/// Workspace existence and writability. The probe file is created at the
/// root and removed — the one write the sweep performs.
fn check_workspace(workspace: &Path, problems: &mut Vec<String>) {
    println!("[doctor] workspace: {}", workspace.display());
    if !workspace.exists() {
        problems.push(format!("workspace {} does not exist", workspace.display()));
        return;
    }
    if !workspace.is_dir() {
        problems.push(format!(
            "workspace {} is not a directory",
            workspace.display()
        ));
        return;
    }
    let probe = workspace.join(".amparo-doctor-probe");
    match std::fs::write(&probe, b"amparo doctor") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            println!("[doctor] workspace writable: yes");
        }
        Err(e) => problems.push(format!(
            "workspace {} is not writable: {e}",
            workspace.display()
        )),
    }
}

/// The privacy ledger: readable, with a row count and no unparseable
/// lines. Missing = a fresh workspace, not a problem.
fn check_ledger(workspace: &Path, problems: &mut Vec<String>) {
    let ledger = privacy_dir(workspace).join("ledger.jsonl");
    if !ledger.exists() {
        println!("[doctor] ledger: none yet");
        return;
    }
    match jsonl_stats(&ledger) {
        Ok((rows, bad)) => {
            if bad > 0 {
                problems.push(format!("ledger: {bad} unparseable line(s)"));
            }
            let bytes = std::fs::metadata(&ledger).map(|m| m.len()).unwrap_or(0);
            println!("[doctor] ledger: {rows} row(s), {bytes} bytes");
        }
        Err(e) => problems.push(format!("ledger unreadable: {e}")),
    }
}

/// Session checkpoints under `<workspace>/.amparo/sessions/` (one
/// tenant subdir per directory level, `:` mapped to `-`). Every `*.json`
/// must parse; a `Running` checkpoint older than 7 days is stale — the
/// resume path skips it, so the operator should resolve it by hand.
fn check_sessions(workspace: &Path, problems: &mut Vec<String>) {
    let dir = workspace.join(".amparo").join("sessions");
    if !dir.exists() {
        println!("[doctor] sessions: none yet");
        return;
    }
    let mut files = Vec::new();
    json_files(&dir, &mut files);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (mut running, mut complete, mut failed, mut corrupt) = (0usize, 0usize, 0usize, 0usize);
    for path in files {
        let checkpoint = match std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|text| serde_json::from_str::<Checkpoint>(&text).map_err(|e| e.to_string()))
        {
            Ok(checkpoint) => checkpoint,
            Err(e) => {
                corrupt += 1;
                problems.push(format!("corrupt checkpoint {}: {e}", path.display()));
                continue;
            }
        };
        match checkpoint.status {
            SessionStatus::Running => {
                running += 1;
                let age = now.saturating_sub(checkpoint.started_at);
                if age > STALE_RUNNING_SECS {
                    let days = age / 86_400;
                    problems.push(format!(
                        "stale Running checkpoint {} ({} days old)",
                        checkpoint.task_id, days
                    ));
                }
            }
            SessionStatus::Complete => complete += 1,
            SessionStatus::Failed => failed += 1,
        }
    }
    println!(
        "[doctor] sessions: {} checkpoint(s) ({} running, {} complete, {} failed, {} corrupt)",
        running + complete + failed + corrupt,
        running,
        complete,
        failed,
        corrupt
    );
}

/// One JSONL directory sweep, shared by the notebook and skills checks:
/// every existing file's lines must parse as JSON (values never printed).
fn check_jsonl_dir(dir: &Path, label: &str, files: &[(&str, &str)], problems: &mut Vec<String>) {
    let mut parts: Vec<String> = Vec::new();
    for (name, file) in files {
        let path = dir.join(file);
        if !path.exists() {
            continue;
        }
        match jsonl_stats(&path) {
            Ok((rows, bad)) => {
                if bad > 0 {
                    problems.push(format!("{label} {file}: {bad} unparseable line(s)"));
                }
                parts.push(format!("{name} {rows}"));
            }
            Err(e) => problems.push(format!("{label} {file} unreadable: {e}")),
        }
    }
    if parts.is_empty() {
        println!("[doctor] {label}: none yet");
    } else {
        println!("[doctor] {label}: {}", parts.join(", "));
    }
}

/// The schedule queue under `<workspace>/.amparo/schedule/`: one JSON
/// file per promise. Files must parse; statuses are counted.
fn check_schedule(workspace: &Path, problems: &mut Vec<String>) {
    let dir = schedule_dir(workspace);
    if !dir.exists() {
        println!("[doctor] schedule: none yet");
        return;
    }
    let mut files = Vec::new();
    json_files(&dir, &mut files);
    let mut counts = std::collections::HashMap::new();
    for path in files {
        match std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|text| {
                serde_json::from_str::<serde_json::Value>(&text).map_err(|e| e.to_string())
            }) {
            Ok(value) => {
                let status = value
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("unknown");
                *counts.entry(status.to_string()).or_insert(0usize) += 1;
            }
            Err(e) => problems.push(format!("schedule file {} unreadable: {e}", path.display())),
        }
    }
    let mut parts: Vec<String> = Vec::new();
    for status in ["pending", "fired", "missed", "cancelled"] {
        if let Some(count) = counts.get(status) {
            parts.push(format!("{status} {count}"));
        }
    }
    let total: usize = counts.values().sum();
    if parts.is_empty() {
        println!("[doctor] schedule: {} file(s)", total);
    } else {
        println!(
            "[doctor] schedule: {} file(s) ({})",
            total,
            parts.join(", ")
        );
    }
}

/// Policy-engine reachability. `--policy-url` dials TCP; `--probe` sends
/// one real `/check` through the wire client (any verdict proves the
/// engine works — only a transport failure is a problem).
async fn check_policy(flags: &DoctorFlags, problems: &mut Vec<String>) {
    let Some(url) = &flags.policy_url else {
        println!("[doctor] policy: not configured (--policy-url)");
        return;
    };
    if let Err(e) = tcp_dial(url) {
        problems.push(format!("policy engine {url} unreachable: {e}"));
        return;
    }
    if flags.probe {
        let api_key = std::env::var("AMPARO_POLICY_KEY").ok();
        let engine = WirePolicyEngine::new(url, api_key);
        let decision = engine
            .judge_tool("doctor_probe", "reachability-check", &[])
            .await;
        let failed = decision
            .fired
            .iter()
            .any(|f| f.starts_with("policy engine failure"));
        let detail = if decision.fired.is_empty() {
            "allow".to_string()
        } else {
            format!("{}", decision.fired.join("; "))
        };
        let verdict = match decision.verdict {
            PolicyVerdict::Allow => "allow",
            PolicyVerdict::Deny => "deny",
            PolicyVerdict::Escalate => "escalate",
        };
        println!("[doctor] policy: {url} answered {verdict} — {detail}");
        if failed {
            problems.push(format!(
                "policy engine {url} answered through the fail-safe path: {detail}"
            ));
        }
    } else {
        println!("[doctor] policy: {url} reachable (tcp)");
    }
}

/// The chat TOML config, when one is named: `ChatConfig::load` applies
/// the same validation the chat host applies at startup.
fn check_chat_config(flags: &DoctorFlags, problems: &mut Vec<String>) {
    let Some(path) = &flags.chat_config else {
        println!("[doctor] chat-config: not configured (--chat-config)");
        return;
    };
    match ChatConfig::load(Path::new(path)) {
        Ok(config) => println!("[doctor] chat-config: {} user(s)", config.users.len()),
        Err(e) => problems.push(format!("chat-config {path}: {e}")),
    }
}

// ─────────────────────────────────────────────── Helpers ─────────────────────

/// Collect every `*.json` file under `dir`, recursively — the sessions
/// and schedule dirs are one level deep (tenant/task names), but the
/// walk stays generic.
fn json_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            json_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
            out.push(path);
        }
    }
}

/// `(rows, unparseable)` for one JSONL file. Empty lines are skipped —
/// they are noise, not corruption.
fn jsonl_stats(path: &Path) -> Result<(usize, usize), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut rows = 0usize;
    let mut bad = 0usize;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        rows += 1;
        if serde_json::from_str::<serde_json::Value>(line).is_err() {
            bad += 1;
        }
    }
    Ok((rows, bad))
}

/// `host:port` from a policy-engine URL. No scheme = http-style port 80;
/// `https://` defaults to 443; a bracketed IPv6 literal is unwrapped.
fn split_host_port(url: &str) -> Option<(String, u16)> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let authority = rest.split('/').next()?;
    if authority.is_empty() {
        return None;
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => {
            let port: u16 = port.parse().ok()?;
            // Unwrap a bracketed IPv6 literal from the host side.
            let host = host
                .strip_prefix('[')
                .and_then(|h| h.strip_suffix(']'))
                .unwrap_or(host);
            if host.is_empty() {
                return None;
            }
            Some((host.to_string(), port))
        }
        None => {
            let port = if url.starts_with("https://") { 443 } else { 80 };
            Some((authority.to_string(), port))
        }
    }
}

/// A 5-second TCP dial — reachability, never an engine check.
fn tcp_dial(url: &str) -> Result<(), String> {
    let (host, port) =
        split_host_port(url).ok_or_else(|| format!("cannot parse a host:port from {url}"))?;
    let addr = format!("{host}:{port}")
        .to_socket_addrs()
        .map_err(|e| format!("{host}:{port}: {e}"))?
        .next()
        .ok_or_else(|| format!("{host}:{port}: no addresses resolved"))?;
    TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .map(|_| ())
        .map_err(|e| format!("{host}:{port}: {e}"))
}

// ───────────────────────────────────────────────────────────── Tests ─────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(args: &[&str]) -> DoctorFlags {
        match parse(args.iter().map(|s| s.to_string()).collect()) {
            ParsedDoctor::Run(flags) => flags,
            ParsedDoctor::Help => panic!("expected flags, got help"),
            ParsedDoctor::Error(message) => panic!("expected flags, got: {message}"),
        }
    }

    fn parse_error(args: &[&str]) -> String {
        match parse(args.iter().map(|s| s.to_string()).collect()) {
            ParsedDoctor::Error(message) => message,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    fn flags() -> DoctorFlags {
        DoctorFlags::default()
    }

    /// One workspace root per test — the tests run in parallel threads of
    /// one process, so a `process::id()`-only name would be shared.
    fn temp_workspace() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!("amparo-doctor-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn help_is_recognized_anywhere() {
        assert!(matches!(parse(vec!["--help".into()]), ParsedDoctor::Help));
        assert!(matches!(parse(vec!["-h".into()]), ParsedDoctor::Help));
        assert!(matches!(
            parse(vec!["--probe".into(), "--help".into()]),
            ParsedDoctor::Help
        ));
    }

    #[test]
    fn parses_all_flags() {
        let flags = parse_ok(&[
            "--workspace",
            "/tmp/ws",
            "--policy-url",
            "https://engine.example",
            "--probe",
            "--chat-config",
            "/tmp/chat.toml",
        ]);
        assert_eq!(flags.workspace.as_deref(), Some("/tmp/ws"));
        assert_eq!(flags.policy_url.as_deref(), Some("https://engine.example"));
        assert!(flags.probe);
        assert_eq!(flags.chat_config.as_deref(), Some("/tmp/chat.toml"));

        let flags = parse_ok(&[]);
        assert!(flags.workspace.is_none());
        assert!(!flags.probe);
    }

    #[test]
    fn rejects_unknown_flags_missing_values_and_probe_without_url() {
        assert!(parse_error(&["--nonsense"]).contains("unknown flag --nonsense"));
        assert!(parse_error(&["--workspace"]).contains("requires a directory"));
        assert!(parse_error(&["--policy-url"]).contains("requires a URL"));
        assert!(parse_error(&["--chat-config"]).contains("requires a path"));
        assert!(parse_error(&["--probe"]).contains("--probe requires --policy-url"));
        assert!(parse_error(&["list"]).contains("takes no positional arguments, got 'list'"));
    }

    #[test]
    fn host_port_parses_schemes_ports_and_ipv6() {
        assert_eq!(
            split_host_port("https://engine.example/check"),
            Some(("engine.example".to_string(), 443))
        );
        assert_eq!(
            split_host_port("http://engine.example:8080"),
            Some(("engine.example".to_string(), 8080))
        );
        assert_eq!(
            split_host_port("http://[::1]:9000/x"),
            Some(("::1".to_string(), 9000))
        );
        assert_eq!(
            split_host_port("engine.example"),
            Some(("engine.example".to_string(), 80))
        );
        assert_eq!(split_host_port("http:///x"), None);
        assert_eq!(split_host_port("http://host:notaport"), None);
    }

    #[tokio::test]
    async fn fresh_workspace_is_healthy() {
        let root = temp_workspace();
        let healthy = run_checks(&root, &flags()).await;
        assert!(healthy);
    }

    #[tokio::test]
    async fn file_as_workspace_is_a_problem() {
        let root = temp_workspace();
        let file = root.join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        let healthy = run_checks(&file, &flags()).await;
        assert!(!healthy);
    }

    #[tokio::test]
    async fn missing_workspace_is_a_problem() {
        let root = temp_workspace();
        let missing = root.join("missing");
        let healthy = run_checks(&missing, &flags()).await;
        assert!(!healthy);
    }

    #[tokio::test]
    async fn unreadable_ledger_is_a_problem() {
        let root = temp_workspace();
        let dir = privacy_dir(&root);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ledger.jsonl"), b"not json").unwrap();
        let healthy = run_checks(&root, &flags()).await;
        assert!(!healthy);
    }

    #[tokio::test]
    async fn stale_running_checkpoint_is_a_problem() {
        let root = temp_workspace();
        let dir = root.join(".amparo").join("sessions").join("cli");
        std::fs::create_dir_all(&dir).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let stale = serde_json::json!({
            "version": 1,
            "tenant": "cli",
            "task_id": "sess-stale",
            "started_at": now - 8 * 86_400,
            "prompt": "old task",
            "status": "running",
            "conversation": [],
            "loop_state": {
                "last_tool_name": null,
                "same_tool_count": 0,
                "empty_turn_retried": false,
                "last_good_summary": null,
                "used_tool_names": [],
                "steps_used": 3,
            },
            "final_answer": null,
        });
        std::fs::write(dir.join("sess-stale.json"), stale.to_string()).unwrap();
        let fresh = serde_json::json!({
            "version": 1,
            "tenant": "cli",
            "task_id": "sess-fresh",
            "started_at": now,
            "prompt": "current task",
            "status": "running",
            "conversation": [],
            "loop_state": {
                "last_tool_name": null,
                "same_tool_count": 0,
                "empty_turn_retried": false,
                "last_good_summary": null,
                "used_tool_names": [],
                "steps_used": 1,
            },
            "final_answer": null,
        });
        std::fs::write(dir.join("sess-fresh.json"), fresh.to_string()).unwrap();
        let healthy = run_checks(&root, &flags()).await;
        assert!(!healthy, "one stale Running checkpoint is a problem");
    }

    #[tokio::test]
    async fn corrupt_checkpoint_is_a_problem() {
        let root = temp_workspace();
        let dir = root.join(".amparo").join("sessions").join("cli");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("sess-broken.json"), b"{not json").unwrap();
        let healthy = run_checks(&root, &flags()).await;
        assert!(!healthy);
    }

    #[tokio::test]
    async fn good_jsonl_and_schedule_are_healthy() {
        let root = temp_workspace();
        let ndir = notebook_dir(&root);
        std::fs::create_dir_all(&ndir).unwrap();
        std::fs::write(
            ndir.join(RECORDS_FILE),
            b"{\"id\":\"a\",\"content\":\"{}\",\"created_at\":\"2026-08-30T00:00:00Z\"}\n",
        )
        .unwrap();
        let sdir = skills_dir(&root);
        std::fs::create_dir_all(&sdir).unwrap();
        std::fs::write(sdir.join(ADOPTED_FILE), b"{\"event\":\"adopt\"}\n\n").unwrap();
        let qdir = schedule_dir(&root);
        std::fs::create_dir_all(&qdir).unwrap();
        std::fs::write(
            qdir.join("sched-1.json"),
            serde_json::json!({"id":"sched-1","status":"pending"}).to_string(),
        )
        .unwrap();
        let healthy = run_checks(&root, &flags()).await;
        assert!(healthy);
    }

    #[tokio::test]
    async fn unparseable_jsonl_line_is_a_problem() {
        let root = temp_workspace();
        let ndir = notebook_dir(&root);
        std::fs::create_dir_all(&ndir).unwrap();
        std::fs::write(ndir.join(HOT_FILE), b"{\"ok\":true}\ngarbage\n").unwrap();
        let healthy = run_checks(&root, &flags()).await;
        assert!(!healthy);
    }

    #[tokio::test]
    async fn chat_config_parses_or_reports() {
        let root = temp_workspace();
        let good = root.join("chat.toml");
        std::fs::write(
            &good,
            b"[users.\"telegram:1\"]\nworkspace = \"users/telegram-1\"\n",
        )
        .unwrap();
        let mut flags = flags();
        flags.chat_config = Some(good.to_string_lossy().to_string());
        assert!(run_checks(&root, &flags).await);

        let bad = root.join("bad.toml");
        std::fs::write(&bad, b"not = [valid").unwrap();
        flags.chat_config = Some(bad.to_string_lossy().to_string());
        assert!(!run_checks(&root, &flags).await);
    }

    #[tokio::test]
    async fn unreachable_engine_is_a_problem() {
        let root = temp_workspace();
        let mut flags = flags();
        // Nothing listens here — the TCP dial must fail fast.
        flags.policy_url = Some("http://127.0.0.1:1".to_string());
        let healthy = run_checks(&root, &flags).await;
        assert!(!healthy);
    }

    #[tokio::test]
    async fn probe_answers_through_a_mock_engine() {
        // One-request HTTP responder, the wire.rs test pattern: any
        // verdict proves the engine works; only transport failures are
        // problems.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            // The sweep dials TCP first, then the probe reconnects — serve
            // both connections, then stop (the dial's client drops it, so
            // its read or write may fail; both are fine).
            for _ in 0..2 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0u8; 8192];
                let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;
                let body = r#"{"verdict":"allow","reason":"read-only"}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = tokio::io::AsyncWriteExt::write_all(&mut sock, resp.as_bytes()).await;
            }
        });

        let root = temp_workspace();
        let mut flags = flags();
        flags.policy_url = Some(format!("http://{addr}"));
        flags.probe = true;
        let healthy = run_checks(&root, &flags).await;
        assert!(healthy, "an answered probe is a healthy engine");
        server.await.unwrap();
    }
}
