// Originally part of Axiom-OS (MIT, Copyright (c) Pixel Phantom AI).
// Ported to Amparo and relicensed Apache-2.0 — see the repository NOTICE.

//! Build tool — detect build system, run builds, parse errors
//!
//! Provides automatic build detection and execution for agentic development workflows.

use super::{ToolCall, ToolExecutor, ToolParam, ToolResult, ToolSchema, ToolTrustTier};
use async_trait::async_trait;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

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

// ─────────────────────────────────────────────────── RunBuildTool ────────────

/// Build tool — runs a build in the Amparo workspace, auto-detecting the build
/// system (Cargo, npm, go, make) or executing an explicit command, and returns
/// parsed errors. Trusted at `SystemControl`.
pub struct RunBuildTool {
    policy: Arc<crate::paths::PathPolicy>,
}
impl RunBuildTool {
    /// Creates a new [`RunBuildTool`].
    pub fn new() -> Self {
        Self::with_policy(Arc::new(crate::paths::PathPolicy::from_env()))
    }

    /// Creates a new [`RunBuildTool`] with an explicitly supplied policy.
    ///
    /// Preferred for consistent configuration across tools (e.g. per-user
    /// workspace roots in chat mode); [`RunBuildTool::new`] reads the
    /// environment instead.
    pub fn with_policy(policy: Arc<crate::paths::PathPolicy>) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl ToolExecutor for RunBuildTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "run_build".to_string(),
            description: "Build the project in the Amparo workspace. Auto-detects the build system (Cargo, npm, go, make) or accepts an explicit command. Returns parsed errors.".to_string(),
            parameters: vec![
                ToolParam {
                    name: "command".to_string(),
                    description: "Optional: explicit build command (e.g. 'cargo build --release'). If omitted, auto-detects.".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: false,
                },
                ToolParam {
                    name: "timeout_seconds".to_string(),
                    description: "Maximum seconds to wait (default 300)".to_string(),
                    param_type: "integer".to_string(),
                    enum_values: None,
                    required: false,
                },
            ],
            trust_tier: ToolTrustTier::SystemControl,
        }
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        let root = self.policy.workspace_root.clone();
        let timeout = call.arg_u64("timeout_seconds").unwrap_or(300);

        let (cmd, args) = if let Some(explicit) = arg_str(call, "command") {
            let parts: Vec<&str> = explicit.split_whitespace().collect();
            if parts.is_empty() {
                return make_result(
                    call,
                    false,
                    serde_json::json!({"error": "empty command"}),
                    "Build failed".to_string(),
                );
            }
            (
                parts[0].to_string(),
                parts[1..].iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            )
        } else {
            detect_build_command(&root).await
        };

        let output = spawn_build(&root, &cmd, &args).output();

        let result = tokio::time::timeout(std::time::Duration::from_secs(timeout), output).await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let combined = format!("{}\n{}", stdout, stderr);
                let errors = parse_build_errors(&combined);

                make_result(
                    call,
                    output.status.success(),
                    serde_json::json!({
                        "success": output.status.success(),
                        "exit_code": output.status.code(),
                        "errors": errors,
                        "error_count": errors.len(),
                        "output": combined.chars().take(4000).collect::<String>(),
                        "command": format!("{} {}", cmd, args.join(" ")),
                    }),
                    if output.status.success() {
                        "Build succeeded".to_string()
                    } else {
                        format!("Build failed: {} error(s)", errors.len())
                    },
                )
            }
            Ok(Err(e)) => make_result(
                call,
                false,
                serde_json::json!({"error": e.to_string()}),
                "Build execution failed".to_string(),
            ),
            Err(_) => make_result(
                call,
                false,
                serde_json::json!({"error": "timeout", "timeout_seconds": timeout}),
                format!("Build timed out after {}s", timeout),
            ),
        }
    }
}

/// The one place a build child is constructed — the confinement
/// (`secret_free_env` so build scripts never inherit an Amparo secret,
/// cwd at the workspace, kill_on_drop) shared by the collecting
/// [`RunBuildTool`] path and the streaming path below so the two can
/// never drift apart.
fn spawn_build(root: &std::path::Path, cmd: &str, args: &[String]) -> Command {
    let mut command = Command::new(cmd);
    command.args(args).current_dir(root).kill_on_drop(true);
    // Build scripts run arbitrary code — the child must not inherit
    // any Amparo secret (audit 2026-08-31 MED-3).
    crate::process_env::secret_free_env(&mut command);
    command
}

/// Streams one build's output into a callback, line by line, and
/// returns the same-shaped [`ToolResult`] [`RunBuildTool`] produces.
///
/// The interactive coding surface's run pane renders through this: the
/// command detection, the confinement and the wall-clock timeout are
/// identical to [`RunBuildTool::execute`], but stdout/stderr lines reach
/// `on_line` as they arrive (`true` marks a stderr line) instead of
/// being collected silently. The first callback line is the command
/// echo (`$ {command}`); the result's `output` field still carries the
/// combined output, error-parsed and truncated exactly as the
/// collecting path truncates it. On timeout the child is killed and the
/// timeout result returned, the collecting path's shape.
pub async fn run_build_streaming(
    policy: &crate::paths::PathPolicy,
    explicit_command: Option<&str>,
    timeout_secs: u64,
    mut on_line: impl FnMut(bool, &str),
) -> ToolResult {
    let root = policy.workspace_root.clone();
    let synthetic = |arguments: Value| ToolCall {
        id: "run_build-stream".to_string(),
        name: "run_build".to_string(),
        arguments,
    };

    let (cmd, args) = match explicit_command {
        Some(explicit) => {
            let parts: Vec<&str> = explicit.split_whitespace().collect();
            if parts.is_empty() {
                return make_result(
                    &synthetic(serde_json::json!({"command": explicit})),
                    false,
                    serde_json::json!({"error": "empty command"}),
                    "Build failed".to_string(),
                );
            }
            (
                parts[0].to_string(),
                parts[1..].iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            )
        }
        None => detect_build_command(&root).await,
    };

    // The workspace may not exist yet (fresh install) — create it so
    // the command has a valid working directory.
    std::fs::create_dir_all(&root).ok();

    let mut child = match spawn_build(&root, &cmd, &args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return make_result(
                &synthetic(serde_json::json!({"command": cmd})),
                false,
                serde_json::json!({"error": e.to_string()}),
                "Build execution failed".to_string(),
            )
        }
    };
    on_line(false, &format!("$ {} {}", cmd, args.join(" ")));

    // Both streams are pumped concurrently — reading one to EOF before
    // touching the other would deadlock once the untouched pipe buffer
    // fills.
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let pump = async {
        let mut out_lines = Box::pin(BufReader::new(stdout).lines());
        let mut err_lines = Box::pin(BufReader::new(stderr).lines());
        let (mut out_buf, mut err_buf) = (String::new(), String::new());
        let (mut out_done, mut err_done) = (false, false);
        while !(out_done && err_done) {
            tokio::select! {
                line = out_lines.next_line(), if !out_done => match line {
                    Ok(Some(l)) => {
                        out_buf.push_str(&l);
                        out_buf.push('\n');
                        on_line(false, &l);
                    }
                    Ok(None) | Err(_) => out_done = true,
                },
                line = err_lines.next_line(), if !err_done => match line {
                    Ok(Some(l)) => {
                        err_buf.push_str(&l);
                        err_buf.push('\n');
                        on_line(true, &l);
                    }
                    Ok(None) | Err(_) => err_done = true,
                },
            }
        }
        (child.wait().await, out_buf, err_buf)
    };

    match tokio::time::timeout(Duration::from_secs(timeout_secs), pump).await {
        // The pump future owns the child — dropping it on expiry kills
        // the process (kill_on_drop above).
        Err(_) => make_result(
            &synthetic(serde_json::json!({"command": cmd})),
            false,
            serde_json::json!({"error": "timeout", "timeout_seconds": timeout_secs}),
            format!("Build timed out after {}s", timeout_secs),
        ),
        Ok((Err(e), _, _)) => make_result(
            &synthetic(serde_json::json!({"command": cmd})),
            false,
            serde_json::json!({"error": e.to_string()}),
            "Build execution failed".to_string(),
        ),
        Ok((Ok(status), out_buf, err_buf)) => {
            let combined = format!("{}\n{}", out_buf, err_buf);
            let errors = parse_build_errors(&combined);
            let success = status.success();
            make_result(
                &synthetic(serde_json::json!({"command": cmd})),
                success,
                serde_json::json!({
                    "success": success,
                    "exit_code": status.code(),
                    "errors": errors,
                    "error_count": errors.len(),
                    "output": combined.chars().take(4000).collect::<String>(),
                    "command": format!("{} {}", cmd, args.join(" ")),
                }),
                if success {
                    "Build succeeded".to_string()
                } else {
                    format!("Build failed: {} error(s)", errors.len())
                },
            )
        }
    }
}

async fn detect_build_command(root: &PathBuf) -> (String, Vec<String>) {
    if root.join("Cargo.toml").exists() {
        return ("cargo".to_string(), vec!["build".to_string()]);
    }
    if root.join("package.json").exists() {
        if root.join("pnpm-lock.yaml").exists() {
            return ("pnpm".to_string(), vec!["build".to_string()]);
        }
        if root.join("yarn.lock").exists() {
            return ("yarn".to_string(), vec!["build".to_string()]);
        }
        return (
            "npm".to_string(),
            vec!["run".to_string(), "build".to_string()],
        );
    }
    if root.join("go.mod").exists() {
        return (
            "go".to_string(),
            vec!["build".to_string(), "./...".to_string()],
        );
    }
    if root.join("Makefile").exists() {
        return ("make".to_string(), vec![]);
    }
    ("cargo".to_string(), vec!["build".to_string()])
}

#[derive(Debug, Clone, serde::Serialize)]
struct BuildError {
    file: String,
    line: Option<u32>,
    message: String,
    severity: String,
}

fn parse_build_errors(output: &str) -> Vec<BuildError> {
    let mut errors = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        // Rust: "error[E0308]: mismatched types" or "error: could not compile"
        if line.starts_with("error") {
            errors.push(BuildError {
                file: String::new(),
                line: None,
                message: line.to_string(),
                severity: "error".to_string(),
            });
        }
        // Go: "./file.go:12:5: error message"
        if let Some(rest) = line.strip_prefix("./") {
            if let Some(colon) = rest.find(':') {
                let file = &rest[..colon];
                let after = &rest[colon + 1..];
                let line_num = after.split(':').next().and_then(|s| s.parse::<u32>().ok());
                let msg_start = after.find(':').map(|i| &after[i + 1..]).unwrap_or(after);
                errors.push(BuildError {
                    file: file.to_string(),
                    line: line_num,
                    message: msg_start.trim().to_string(),
                    severity: "error".to_string(),
                });
            }
        }
        // TypeScript/JS: "src/file.ts(10,5): error TS2322: ..."
        if line.contains(".ts(") || line.contains(".js(") {
            if line.contains("error") {
                errors.push(BuildError {
                    file: String::new(),
                    line: None,
                    message: line.to_string(),
                    severity: "error".to_string(),
                });
            }
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_policy() -> Arc<crate::paths::PathPolicy> {
        Arc::new(crate::paths::PathPolicy {
            workspace_root: std::env::temp_dir().join("amparo-test-build-ws"),
            max_execution_seconds: 30,
            ..Default::default()
        })
    }

    #[test]
    fn parse_rust_errors() {
        let output =
            "error[E0308]: mismatched types\n  --> src/main.rs:10:5\nerror: could not compile";
        let errors = parse_build_errors(output);
        assert!(errors.len() >= 2);
    }

    #[test]
    fn parse_go_errors() {
        let output = "./main.go:12:5: undefined: foo\n./util.go:30:2: missing return";
        let errors = parse_build_errors(output);
        assert!(errors.len() >= 2);
        assert_eq!(errors[0].file, "main.go");
        assert_eq!(errors[0].line, Some(12));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_streaming_explicit_command_delivers_lines() {
        let policy = test_policy();
        let mut lines: Vec<(bool, String)> = Vec::new();
        let result = run_build_streaming(&policy, Some("echo build-marker"), 30, |is_stderr, l| {
            lines.push((is_stderr, l.to_string()));
        })
        .await;
        assert!(result.success, "output: {}", result.output);
        assert!(result.output.to_string().contains("build-marker"));
        // First callback line is the command echo.
        assert_eq!(lines[0], (false, "$ echo build-marker".to_string()));
        // The command's stdout line arrives flagged as stdout.
        assert!(lines.iter().any(|(err, l)| !err && l == "build-marker"));
        assert_eq!(result.output["command"], "echo build-marker");
        assert_eq!(result.output["error_count"], 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_streaming_auto_detect_falls_back_to_cargo() {
        let policy = test_policy();
        // A fresh workspace with no build files: detection falls back to
        // `cargo build`, which fails against the missing Cargo.toml.
        let result = run_build_streaming(&policy, None, 120, |_is_stderr, _l| {}).await;
        assert!(!result.success);
        assert_eq!(result.output["exit_code"], 101);
        assert!(result.output["error_count"].as_u64().unwrap() >= 1);
        assert!(result.output["command"]
            .as_str()
            .unwrap()
            .starts_with("cargo build"));
        assert!(result.output.to_string().contains("Cargo.toml"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_streaming_timeout_kills() {
        let policy = test_policy();
        let mut lines: Vec<(bool, String)> = Vec::new();
        // A long-running command in each platform's shell: cmd has no
        // `sleep`; `ping -n` is the classic Windows stand-in.
        #[cfg(unix)]
        let long_running = "sleep 30";
        #[cfg(windows)]
        let long_running = "ping -n 31 127.0.0.1";
        let result = run_build_streaming(&policy, Some(long_running), 1, |is_stderr, l| {
            lines.push((is_stderr, l.to_string()));
        })
        .await;
        assert!(!result.success);
        assert_eq!(result.output["error"], "timeout");
        // The echo line was delivered before the timeout killed the child.
        assert_eq!(lines[0].1, format!("$ {long_running}"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_streaming_empty_command() {
        let policy = test_policy();
        let mut lines: Vec<(bool, String)> = Vec::new();
        let result = run_build_streaming(&policy, Some("   "), 30, |is_stderr, l| {
            lines.push((is_stderr, l.to_string()));
        })
        .await;
        assert!(!result.success);
        assert_eq!(result.output["error"], "empty command");
        // Nothing spawned — not even the echo line.
        assert!(lines.is_empty());
    }
}
