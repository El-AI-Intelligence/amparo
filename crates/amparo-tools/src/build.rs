// Originally part of Axiom-OS (MIT, Copyright (c) Pixel Phantom AI).
// Ported to Amparo and relicensed Apache-2.0 — see the repository NOTICE.

//! Build tool — detect build system, run builds, parse errors
//!
//! Provides automatic build detection and execution for agentic development workflows.

use super::{ToolCall, ToolExecutor, ToolParam, ToolResult, ToolSchema, ToolTrustTier};
use async_trait::async_trait;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
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

        let output = Command::new(&cmd)
            .args(&args)
            .current_dir(&root)
            .kill_on_drop(true)
            .output();

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
}
