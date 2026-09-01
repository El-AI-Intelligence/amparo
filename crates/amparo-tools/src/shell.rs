// Originally part of Axiom-OS (MIT, Copyright (c) Pixel Phantom AI) —
// `tools/shell.rs` (Layer 3.2).
// Ported to Amparo and relicensed Apache-2.0 — see the repository NOTICE.
//
// Layered defense:
// 1. **Policy gate**: the agent loop's deny-by-default PolicyEngine judges
//    every call before this tool is reached (amparo-policy).
// 2. **Blocklist** (defense-in-depth): catch-all patterns checked by
//    PathPolicy before any command spawns.
// 3. **Limits**: wall-clock timeout per command.
//
// OS-level confinement (bwrap/unshare namespaces) is scheduled for the
// hardening milestone — see `paths.rs`.
//
// Blocked: rm -rf, sudo, su, passwd, chown, chmod, mkfs, dd, fork bombs,
// pipe-to-shell, base64-encoded evasion.

//! Shell tool — execute commands inside the path policy's boundaries.
//!
//! `run_command` runs commands via `bash` with layered defense: the
//! deny-by-default policy gate in `amparo-agent` judges every call first, the
//! [`PathPolicy`] blocklist rejects destructive patterns before anything
//! spawns, and a wall-clock timeout bounds every command. OS-level confinement
//! (bwrap/unshare namespaces) is scheduled for the hardening milestone.

use super::{ToolCall, ToolExecutor, ToolParam, ToolResult, ToolSchema, ToolTrustTier};
use crate::paths::PathPolicy;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
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

/// RunCommandTool — execute shell commands within the path policy's boundaries.
pub struct RunCommandTool {
    policy: Arc<PathPolicy>,
}

impl RunCommandTool {
    /// Creates a shell tool with the path policy loaded from environment
    /// variables.
    pub fn new() -> Self {
        Self {
            policy: Arc::new(PathPolicy::from_env()),
        }
    }

    /// Create with a shared PathPolicy (preferred for consistent config).
    pub fn with_policy(policy: Arc<PathPolicy>) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl ToolExecutor for RunCommandTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "run_command".to_string(),
            description: "Execute a shell command inside the Amparo workspace. Commands run with the workspace as their working directory and home, with a wall-clock timeout (default 30s, max 120s). Destructive commands (rm -rf /, sudo, etc.) are always blocked; every call must pass the policy gate first.".to_string(),
            parameters: vec![
                ToolParam {
                    name: "command".to_string(),
                    description: "The shell command to execute".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: true,
                },
                ToolParam {
                    name: "timeout_secs".to_string(),
                    description: "Maximum execution time in seconds (default 30, max 120)".to_string(),
                    param_type: "integer".to_string(),
                    enum_values: None,
                    required: false,
                },
            ],
            trust_tier: ToolTrustTier::ExternalEffector,
        }
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        let command = match call.arguments.get("command").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => {
                return make_result(
                    call,
                    false,
                    serde_json::json!({"error": "missing command"}),
                    "Command failed".to_string(),
                )
            }
        };

        // ── Blocklist (defense-in-depth) ──────────────────────────────
        if let Some(reason) = self.policy.check_command_blocked(&command) {
            return make_result(
                call,
                false,
                serde_json::json!({
                    "command": command,
                    "blocked": true,
                    "block_reason": reason,
                }),
                format!("Blocked: {}", reason),
            );
        }

        let requested = call.arg_u64("timeout_secs").unwrap_or(30);
        let timeout_secs = requested.min(self.policy.max_execution_seconds);
        let workspace = self.policy.workspace_root.clone();
        // The workspace may not exist yet (fresh install) — create it so the
        // command has a valid working directory.
        std::fs::create_dir_all(&workspace).ok();

        // ── Execute with wall-clock timeout ────────────────────────────
        #[cfg(not(windows))]
        let run = async {
            Command::new("bash")
                .arg("-c")
                .arg(&command)
                .env_clear()
                .env("PATH", "/usr/bin:/bin:/usr/local/bin")
                .env("HOME", workspace.to_string_lossy().to_string())
                .current_dir(&workspace)
                .output()
                .await
        };
        // On Windows there is no `bash` on PATH — the name resolves to the
        // WSL shim, which fails without a WSL distro. Run through `cmd /C`
        // instead (cmd has no `pwd`; `cd` with no arguments prints the
        // current directory, the same echo of the cwd the shell tool relies
        // on).
        #[cfg(windows)]
        let run = async {
            Command::new("cmd")
                .arg("/C")
                .arg(&command)
                .env_clear()
                .env("PATH", "C:\\Windows\\System32;C:\\Windows")
                .current_dir(&workspace)
                .output()
                .await
        };

        match tokio::time::timeout(Duration::from_secs(timeout_secs), run).await {
            Err(_) => make_result(
                call,
                false,
                serde_json::json!({
                    "command": command,
                    "error": format!("timed out after {}s", timeout_secs),
                }),
                format!("Timed out after {}s", timeout_secs),
            ),
            Ok(Err(e)) => make_result(
                call,
                false,
                serde_json::json!({"error": e.to_string(), "command": command}),
                format!("Command failed to spawn: {}", e),
            ),
            Ok(Ok(out)) => {
                let success = out.status.success();
                let exit_code = out.status.code().unwrap_or(-1);
                let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                let summary = if success {
                    format!("Command completed (exit 0)")
                } else {
                    format!("Command failed (exit {})", exit_code)
                };
                make_result(
                    call,
                    success,
                    serde_json::json!({
                        "command": command,
                        "exit_code": exit_code,
                        "stdout": &stdout[..stdout.len().min(6000)],
                        "stderr": &stderr[..stderr.len().min(2000)],
                        "blocked": false,
                    }),
                    summary,
                )
            }
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_call(name: &str, command: &str) -> ToolCall {
        ToolCall {
            id: "test-1".to_string(),
            name: name.to_string(),
            arguments: serde_json::json!({"command": command}),
        }
    }

    fn test_policy() -> Arc<PathPolicy> {
        Arc::new(PathPolicy {
            workspace_root: std::env::temp_dir().join("amparo-test-ws"),
            max_execution_seconds: 30,
            ..Default::default()
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_echo_succeeds() {
        let tool = RunCommandTool::with_policy(test_policy());
        let call = tool_call("run_command", "echo hello world");
        let result = tool.execute(&call).await;
        assert!(result.success, "output: {}", result.output);
        assert!(result.output.to_string().contains("hello world"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_blocked_sudo() {
        let tool = RunCommandTool::with_policy(test_policy());
        let call = tool_call("run_command", "sudo rm -rf /");
        let result = tool.execute(&call).await;
        assert!(!result.success);
        assert_eq!(result.output["blocked"], true);
        assert!(result.output["block_reason"].as_str().is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_pipe_to_bash_blocked() {
        let tool = RunCommandTool::with_policy(test_policy());
        let call = tool_call("run_command", "echo x | bash");
        let result = tool.execute(&call).await;
        assert!(!result.success);
        assert_eq!(result.output["blocked"], true);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_timeout_kills() {
        let tool = RunCommandTool::with_policy(test_policy());
        let call = ToolCall {
            id: "t".to_string(),
            name: "run_command".to_string(),
            arguments: serde_json::json!({"command": "sleep 30", "timeout_secs": 1}),
        };
        let result = tool.execute(&call).await;
        assert!(!result.success);
        assert!(result.output.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn test_missing_command() {
        let tool = RunCommandTool::new();
        let call = ToolCall {
            id: "test-2".to_string(),
            name: "run_command".to_string(),
            arguments: serde_json::json!({}),
        };
        let result = tool.execute(&call).await;
        assert!(!result.success);
    }
}
