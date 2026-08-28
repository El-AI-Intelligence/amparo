//! Real-process e2e for the shipped `amparo-mcp-serve` binary.
//!
//! The in-crate duplex tests cover the protocol path; these cover the
//! artifact a user actually runs: flag parsing, the deny-by-default posture
//! with no flags, a real execution under `--allow-all`, the trust ceiling,
//! and the approval default (auto-deny for external-effector calls).
//!
//! `AMPARO_WORKSPACE` is process-wide state, so the tests that spawn the
//! binary set it around child spawns and hold [`LOCK`] to serialize.

use amparo_mcp::McpClient;
use serde_json::json;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

fn bin() -> String {
    // Set at test runtime by cargo (this cargo version does not expose it
    // at compile time via env!, and keeps the dash in the name).
    std::env::var("CARGO_BIN_EXE_amparo-mcp-serve")
        .expect("cargo sets CARGO_BIN_EXE_<bin-name> for tests")
}

/// Serializes the tests that mutate `AMPARO_WORKSPACE` around child spawns.
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// One shared workspace for every spawned binary (created once per test
/// process; the tools create parent dirs themselves where needed).
fn workspace() -> &'static PathBuf {
    static WS: OnceLock<PathBuf> = OnceLock::new();
    WS.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("amparo-mcp-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("workspace dir");
        dir
    })
}

/// Point child processes at the shared workspace; returns the prior value
/// for restoration. Callers hold `LOCK`.
fn set_workspace_env() -> Option<String> {
    let prior = std::env::var("AMPARO_WORKSPACE").ok();
    std::env::set_var("AMPARO_WORKSPACE", workspace());
    prior
}

fn restore_workspace_env(prior: Option<String>) {
    match prior {
        Some(v) => std::env::set_var("AMPARO_WORKSPACE", v),
        None => std::env::remove_var("AMPARO_WORKSPACE"),
    }
}

fn first_text(result: &amparo_mcp::CallToolResult) -> &str {
    result
        .content
        .iter()
        .find(|c| c.block_type == "text")
        .map(|c| c.text.as_str())
        .unwrap_or_default()
}

// ── CLI surface ──────────────────────────────────────────────────────────────

#[test]
fn cli_flags_parse() {
    let help = Command::new(bin()).arg("--help").output().unwrap();
    assert!(help.status.success());
    let stdout = String::from_utf8_lossy(&help.stdout);
    assert!(stdout.contains("amparo-mcp-serve"));
    assert!(stdout.contains("--trust-ceiling"));

    let unknown = Command::new(bin()).arg("--nonsense").output().unwrap();
    assert_eq!(unknown.status.code(), Some(2), "unknown flags are rejected");

    let conflict = Command::new(bin())
        .args(["--policy-url", "http://example.test", "--allow-all"])
        .output()
        .unwrap();
    assert_eq!(conflict.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&conflict.stderr).contains("mutually exclusive"),
        "--policy-url and --allow-all cannot combine"
    );

    let bad_tier = Command::new(bin())
        .args(["--trust-ceiling", "nonsense"])
        .output()
        .unwrap();
    assert_eq!(bad_tier.status.code(), Some(2));
}

// ── Process e2e ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn no_flags_denies_every_call() {
    let _guard = LOCK.lock().await;
    let client = McpClient::spawn(bin(), std::iter::empty::<&str>()).await.unwrap();
    let names: Vec<&str> = client.tools().iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"list_dir"), "registry is listed even with no policy");
    assert!(names.contains(&"run_command"));

    let result = client.call_tool("list_dir", json!({"path": "."})).await.unwrap();
    assert!(result.isError, "a no-flags server must deny every call");
    let text = first_text(&result);
    assert!(text.contains("Policy denied list_dir"), "denial names the tool: {text}");
    assert!(text.contains("no policy configured"), "denial carries the reason: {text}");
}

#[tokio::test]
async fn allow_all_executes_a_benign_tool() {
    let _guard = LOCK.lock().await;
    let prior = set_workspace_env();
    std::fs::write(workspace().join("marker.txt"), "amparo e2e").unwrap();

    let client = McpClient::spawn(bin(), ["--allow-all"]).await.unwrap();
    let result = client.call_tool("list_dir", json!({"path": "."})).await.unwrap();
    assert!(!result.isError, "list_dir should run under --allow-all: {}", first_text(&result));
    assert!(
        first_text(&result).contains("marker.txt"),
        "the real tool ran against the real workspace: {}",
        first_text(&result)
    );

    restore_workspace_env(prior);
}

#[tokio::test]
async fn trust_ceiling_blocks_mutating_tools() {
    let _guard = LOCK.lock().await;
    let prior = set_workspace_env();

    let client = McpClient::spawn(
        bin(),
        ["--allow-all", "--trust-ceiling", "observational"],
    )
    .await
    .unwrap();
    let result = client
        .call_tool("write_file", json!({"path": "x.txt", "content": "hi"}))
        .await
        .unwrap();
    assert!(result.isError, "a mutating tool must not pass an observational ceiling");
    assert!(first_text(&result).contains("trust ceiling"), "{}", first_text(&result));
    assert!(
        !workspace().join("x.txt").exists(),
        "tools blocked by the ceiling never execute"
    );

    restore_workspace_env(prior);
}

#[tokio::test]
async fn external_effector_needs_approval_by_default() {
    let _guard = LOCK.lock().await;
    let prior = set_workspace_env();

    // --allow-all lets the policy pass but approval still auto-denies: an
    // MCP client has no human at the terminal unless --auto-approve says so.
    let client = McpClient::spawn(bin(), ["--allow-all"]).await.unwrap();
    let result = client
        .call_tool("run_command", json!({"command": "echo hi"}))
        .await
        .unwrap();
    assert!(result.isError, "external-effector calls must ask approval first");
    assert!(first_text(&result).contains("User denied"), "{}", first_text(&result));

    restore_workspace_env(prior);
}

#[tokio::test]
async fn auto_approve_runs_an_external_effector() {
    let _guard = LOCK.lock().await;
    let prior = set_workspace_env();
    let marker = format!("amparo-e2e-{}", std::process::id());

    let client = McpClient::spawn(bin(), ["--allow-all", "--auto-approve"]).await.unwrap();
    let result = client
        .call_tool("run_command", json!({"command": format!("echo {marker}")}))
        .await
        .unwrap();
    assert!(!result.isError, "an approved command should run: {}", first_text(&result));
    assert!(
        first_text(&result).contains(&marker),
        "the command's stdout comes back through the protocol: {}",
        first_text(&result)
    );

    restore_workspace_env(prior);
}
