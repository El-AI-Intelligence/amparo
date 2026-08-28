// Originally part of Axiom-OS (MIT, Copyright (c) Pixel Phantom AI).
// Ported to Amparo and relicensed Apache-2.0 — see the repository NOTICE.

//! Test runner tools — detect test framework, run tests, parse output
//!
//! Provides automatic test detection and execution for agentic development workflows.

use super::{ToolCall, ToolExecutor, ToolParam, ToolResult, ToolSchema, ToolTrustTier};
use async_trait::async_trait;
use serde_json::Value;
use std::path::PathBuf;
use tokio::process::Command;

fn sandbox_root() -> PathBuf {
    if let Ok(dir) = std::env::var("AMPARO_WORKSPACE") {
        PathBuf::from(dir)
    } else {
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"))
            .join("amparo-workspace")
    }
}

fn make_result(call: &ToolCall, success: bool, output: Value, summary: String) -> ToolResult {
    ToolResult {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        success,
        output,
        display_summary: summary,
        duration_ms: 0,
    }
}

fn arg_str<'a>(call: &'a ToolCall, key: &str) -> Option<&'a str> {
    call.arguments.get(key).and_then(|v| v.as_str())
}

// ─────────────────────────────────────────────────── RunTestsTool ────────────
/// Detects the test framework from the workspace and runs tests.
/// Supports: cargo test, npm test, pytest, go test, jest, vitest.

pub struct RunTestsTool;
impl RunTestsTool { pub fn new() -> Self { Self } }

#[async_trait]
impl ToolExecutor for RunTestsTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "run_tests".to_string(),
            description: "Run tests in the Amparo workspace. Auto-detects the test framework (Cargo, npm, pytest, go, jest) or accepts an explicit command. Returns parsed results with pass/fail counts.".to_string(),
            parameters: vec![
                ToolParam {
                    name: "command".to_string(),
                    description: "Optional: explicit test command (e.g. 'cargo test -p axiom-engram'). If omitted, auto-detects from workspace.".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: false,
                },
                ToolParam {
                    name: "timeout_seconds".to_string(),
                    description: "Maximum seconds to wait for tests (default 120)".to_string(),
                    param_type: "integer".to_string(),
                    enum_values: None,
                    required: false,
                },
            ],
            trust_tier: ToolTrustTier::SystemControl,
        }
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        let root = sandbox_root();
        let timeout = call.arg_u64("timeout_seconds")
            .unwrap_or(120);

        let (cmd, args) = if let Some(explicit) = arg_str(call, "command") {
            let parts: Vec<&str> = explicit.split_whitespace().collect();
            if parts.is_empty() {
                return make_result(call, false, serde_json::json!({"error": "empty command"}), "Test failed".to_string());
            }
            (parts[0].to_string(), parts[1..].iter().map(|s| s.to_string()).collect::<Vec<_>>())
        } else {
            detect_test_command(&root).await
        };

        let output = Command::new(&cmd)
            .args(&args)
            .current_dir(&root)
            .kill_on_drop(true)
            .output();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout),
            output,
        ).await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let combined = format!("{}\n{}", stdout, stderr);
                let parsed = parse_test_output(&combined);

                make_result(call, output.status.success(), serde_json::json!({
                    "success": output.status.success(),
                    "exit_code": output.status.code(),
                    "passed": parsed.passed,
                    "failed": parsed.failed,
                    "skipped": parsed.skipped,
                    "total": parsed.total,
                    "duration_ms": parsed.duration_ms,
                    "failures": parsed.failures,
                    "output": combined.chars().take(4000).collect::<String>(),
                    "command": format!("{} {}", cmd, args.join(" ")),
                }), format!("Tests: {}/{} passed{}",
                    parsed.passed, parsed.total,
                    if parsed.failed > 0 { format!(", {} failed", parsed.failed) } else { String::new() }
                ))
            }
            Ok(Err(e)) => make_result(call, false, serde_json::json!({"error": e.to_string()}), "Test execution failed".to_string()),
            Err(_) => make_result(call, false, serde_json::json!({"error": "timeout", "timeout_seconds": timeout}), format!("Tests timed out after {}s", timeout)),
        }
    }
}

async fn detect_test_command(root: &PathBuf) -> (String, Vec<String>) {
    if root.join("Cargo.toml").exists() {
        return ("cargo".to_string(), vec!["test".to_string(), "--no-fail-fast".to_string()]);
    }
    if root.join("package.json").exists() {
        if root.join("pnpm-lock.yaml").exists() {
            return ("pnpm".to_string(), vec!["test".to_string()]);
        }
        if root.join("yarn.lock").exists() {
            return ("yarn".to_string(), vec!["test".to_string()]);
        }
        return ("npm".to_string(), vec!["test".to_string()]);
    }
    if root.join("go.mod").exists() {
        return ("go".to_string(), vec!["test".to_string(), "./...".to_string()]);
    }
    if root.join("pytest.ini").exists() || root.join("pyproject.toml").exists() {
        return ("python".to_string(), vec!["-m".to_string(), "pytest".to_string(), "-v".to_string()]);
    }
    ("cargo".to_string(), vec!["test".to_string()])
}

struct ParsedTestOutput {
    passed: usize,
    failed: usize,
    skipped: usize,
    total: usize,
    duration_ms: u64,
    failures: Vec<String>,
}

fn parse_test_output(output: &str) -> ParsedTestOutput {
    let mut passed = 0;
    let mut failed = 0;
    let mut skipped = 0;
    let mut duration_ms = 0;
    let mut failures = Vec::new();

    for line in output.lines() {
        let line = line.trim();

        // Cargo test: "test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out"
        // Also handles: "test result: FAILED. 4 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out"
        if line.starts_with("test result:") {
            for part in line.split(';') {
                let part = part.trim();
                let tokens: Vec<&str> = part.split_whitespace().collect();
                for (i, tok) in tokens.iter().enumerate() {
                    if let Ok(n) = tok.parse::<usize>() {
                        if let Some(next) = tokens.get(i + 1) {
                            if next.contains("passed") { passed = n; }
                            else if next.contains("failed") { failed = n; }
                            else if next.contains("ignored") { skipped = n; }
                        }
                        if i > 0 {
                            let prev = tokens[i - 1];
                            if prev.contains("passed") { passed = n; }
                            else if prev.contains("failed") { failed = n; }
                            else if prev.contains("ignored") { skipped = n; }
                        }
                    }
                }
            }
        }

        // Cargo test failures: "---- test_name ----"
        if line.starts_with("----") && line.ends_with("----") {
            let name = line.trim_matches('-').trim();
            if !name.is_empty() && !name.contains(' ') {
                failures.push(name.to_string());
            }
        }

        // npm/jest: "Tests: 5 passed, 2 failed, 7 total"
        // Handles extra spaces and various formats
        if line.contains("Tests:") && (line.contains("passed") || line.contains("failed")) {
            let after_colon = line.split(':').nth(1).unwrap_or("");
            for part in after_colon.split(',') {
                let part = part.trim();
                let tokens: Vec<&str> = part.split_whitespace().collect();
                if let Some(first) = tokens.first() {
                    if let Ok(n) = first.parse::<usize>() {
                        let joined = tokens[1..].join(" ");
                        if joined.contains("passed") { passed = n; }
                        else if joined.contains("failed") { failed = n; }
                    }
                }
            }
        }

        // pytest: "=== 5 passed, 2 failed in 1.23s ==="
        if line.starts_with("===") && (line.contains("passed") || line.contains("failed")) {
            let inner = line.trim_matches('=').trim();
            for part in inner.split(',') {
                let part = part.trim();
                let tokens: Vec<&str> = part.split_whitespace().collect();
                if let Some(first) = tokens.first() {
                    if let Ok(n) = first.parse::<usize>() {
                        let rest = tokens[1..].join(" ");
                        if rest.contains("passed") { passed = n; }
                        else if rest.contains("failed") { failed = n; }
                        else if rest.contains("skipped") { skipped = n; }
                    }
                }
                if let Some(s_pos) = part.find(" in ") {
                    let dur_str = &part[s_pos + 4..].trim_end_matches('s');
                    if let Ok(secs) = dur_str.parse::<f64>() {
                        duration_ms = (secs * 1000.0) as u64;
                    }
                }
            }
        }

        // Go test: "ok  	mypackage	0.123s" / "FAIL	mypackage	0.456s"
        if line.starts_with("ok\t") || line.starts_with("FAIL\t") {
            if line.starts_with("ok\t") { passed += 1; }
            else { failed += 1; }
        }

        // Duration patterns
        if line.contains("Finished in") || line.contains("Ran") {
            if let Some(s_pos) = line.rfind(" in ") {
                let dur_str = &line[s_pos + 4..].trim_end_matches('s');
                if let Ok(secs) = dur_str.parse::<f64>() {
                    duration_ms = (secs * 1000.0) as u64;
                }
            }
        }
    }

    let total = passed + failed + skipped;
    ParsedTestOutput { passed, failed, skipped, total, duration_ms, failures }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cargo_test_output() {
        let output = "running 5 tests\ntest test_one ... ok\ntest test_two ... FAILED\n\ntest result: FAILED. 4 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out";
        let parsed = parse_test_output(output);
        assert_eq!(parsed.passed, 4);
        assert_eq!(parsed.failed, 1);
        assert_eq!(parsed.total, 5);
    }

    #[test]
    fn parse_pytest_output() {
        let output = "=== 3 passed, 1 failed in 0.52s ===";
        let parsed = parse_test_output(output);
        assert_eq!(parsed.passed, 3);
        assert_eq!(parsed.failed, 1);
        assert_eq!(parsed.total, 4);
        assert!(parsed.duration_ms > 0);
    }

    #[test]
    fn parse_npm_test_output() {
        let output = "Tests:   5 passed, 2 failed, 7 total";
        let parsed = parse_test_output(output);
        assert_eq!(parsed.passed, 5);
        assert_eq!(parsed.failed, 2);
    }
}