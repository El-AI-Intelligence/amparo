// Originally part of Axiom-OS (MIT, Copyright (c) Pixel Phantom AI).
// Ported to Amparo and relicensed Apache-2.0 — see the repository NOTICE.

//! Git tools — status, diff, commit, branch, log, blame
//!
//! Provides git operations within the Amparo workspace for agentic development workflows.

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

async fn run_git(args: &[&str], cwd: &PathBuf) -> std::result::Result<String, String> {
    let mut command = Command::new("git");
    command.args(args).current_dir(cwd);
    // Git hooks run arbitrary code — the child must not inherit any
    // Amparo secret (audit 2026-08-31 MED-3).
    crate::process_env::secret_free_env(&mut command);
    let output = command
        .output()
        .await
        .map_err(|e| format!("Failed to run git: {}", e))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}

// ─────────────────────────────────────────────────── GitStatusTool ───────────

/// Shows the working-tree status of the git repository in the Amparo
/// workspace. Trusted at `Observational`.
pub struct GitStatusTool {
    policy: Arc<crate::paths::PathPolicy>,
}
impl GitStatusTool {
    /// Creates a new [`GitStatusTool`].
    pub fn new() -> Self {
        Self::with_policy(Arc::new(crate::paths::PathPolicy::from_env()))
    }

    /// Creates a new [`GitStatusTool`] with an explicitly supplied policy.
    ///
    /// Preferred for consistent configuration across tools (e.g. per-user
    /// workspace roots in chat mode); [`GitStatusTool::new`] reads the
    /// environment instead.
    pub fn with_policy(policy: Arc<crate::paths::PathPolicy>) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl ToolExecutor for GitStatusTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "git_status".to_string(),
            description: "Show the working tree status of the Amparo workspace git repository. Shows modified, added, deleted, and untracked files.".to_string(),
            parameters: vec![],
            trust_tier: ToolTrustTier::Observational,
        }
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        let root = self.policy.workspace_root.clone();
        match run_git(&["status", "--porcelain", "-b"], &root).await {
            Ok(output) => {
                let lines: Vec<&str> = output.lines().collect();
                let modified = lines
                    .iter()
                    .filter(|l| l.starts_with(" M") || l.starts_with("M "))
                    .count();
                let added = lines
                    .iter()
                    .filter(|l| l.starts_with("A ") || l.starts_with("??"))
                    .count();
                let deleted = lines
                    .iter()
                    .filter(|l| l.starts_with(" D") || l.starts_with("D "))
                    .count();
                let branch = lines
                    .first()
                    .and_then(|l| l.strip_prefix("## "))
                    .unwrap_or("unknown");
                make_result(
                    call,
                    true,
                    serde_json::json!({
                        "branch": branch,
                        "modified": modified,
                        "added": added,
                        "deleted": deleted,
                        "raw": output,
                        "total_changes": modified + added + deleted,
                    }),
                    format!("{} changes on {}", modified + added + deleted, branch),
                )
            }
            Err(e) => make_result(
                call,
                false,
                serde_json::json!({"error": e}),
                "Git status failed".to_string(),
            ),
        }
    }
}

// ─────────────────────────────────────────────────── GitDiffTool ─────────────

/// Shows the diff of working-tree changes (staged, unstaged, or between
/// commits) in the workspace repository. Trusted at `Observational`.
pub struct GitDiffTool {
    policy: Arc<crate::paths::PathPolicy>,
}
impl GitDiffTool {
    /// Creates a new [`GitDiffTool`].
    pub fn new() -> Self {
        Self::with_policy(Arc::new(crate::paths::PathPolicy::from_env()))
    }

    /// Creates a new [`GitDiffTool`] with an explicitly supplied policy.
    ///
    /// Preferred for consistent configuration across tools (e.g. per-user
    /// workspace roots in chat mode); [`GitDiffTool::new`] reads the
    /// environment instead.
    pub fn with_policy(policy: Arc<crate::paths::PathPolicy>) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl ToolExecutor for GitDiffTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "git_diff".to_string(),
            description: "Show the diff of changes in the Amparo workspace. Can show staged or unstaged changes, or diff between commits.".to_string(),
            parameters: vec![
                ToolParam {
                    name: "staged".to_string(),
                    description: "If true, show staged changes (git diff --cached). Default: false (unstaged changes)".to_string(),
                    param_type: "boolean".to_string(),
                    enum_values: None,
                    required: false,
                },
                ToolParam {
                    name: "file".to_string(),
                    description: "Optional: limit diff to a specific file path".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: false,
                },
                ToolParam {
                    name: "max_lines".to_string(),
                    description: "Maximum lines to return (default 500)".to_string(),
                    param_type: "integer".to_string(),
                    enum_values: None,
                    required: false,
                },
            ],
            trust_tier: ToolTrustTier::Observational,
        }
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        let root = self.policy.workspace_root.clone();
        let staged = call.arg_bool("staged").unwrap_or(false);
        let file = arg_str(call, "file");
        let max_lines = call.arg_u64("max_lines").unwrap_or(500) as usize;

        let mut args = vec!["diff"];
        if staged {
            args.push("--cached");
        }
        if let Some(f) = file {
            args.push(f);
        }

        match run_git(&args, &root).await {
            Ok(output) => {
                let lines: Vec<&str> = output.lines().collect();
                let truncated = lines.len() > max_lines;
                let display = if truncated {
                    lines[..max_lines].join("\n") + "\n... [truncated]"
                } else {
                    output.clone()
                };
                make_result(
                    call,
                    true,
                    serde_json::json!({
                        "diff": display,
                        "total_lines": lines.len(),
                        "truncated": truncated,
                        "staged": staged,
                    }),
                    format!(
                        "Diff: {} lines{}",
                        lines.len(),
                        if truncated { " (truncated)" } else { "" }
                    ),
                )
            }
            Err(e) => make_result(
                call,
                false,
                serde_json::json!({"error": e}),
                "Git diff failed".to_string(),
            ),
        }
    }
}

// ─────────────────────────────────────────────────── GitCommitTool ───────────

/// Stages all changes and creates a commit in the workspace repository.
/// Trusted at `LocalMutating`.
pub struct GitCommitTool {
    policy: Arc<crate::paths::PathPolicy>,
}
impl GitCommitTool {
    /// Creates a new [`GitCommitTool`].
    pub fn new() -> Self {
        Self::with_policy(Arc::new(crate::paths::PathPolicy::from_env()))
    }

    /// Creates a new [`GitCommitTool`] with an explicitly supplied policy.
    ///
    /// Preferred for consistent configuration across tools (e.g. per-user
    /// workspace roots in chat mode); [`GitCommitTool::new`] reads the
    /// environment instead.
    pub fn with_policy(policy: Arc<crate::paths::PathPolicy>) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl ToolExecutor for GitCommitTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "git_commit".to_string(),
            description: "Stage all changes and create a git commit with a message. This is a mutating operation.".to_string(),
            parameters: vec![
                ToolParam {
                    name: "message".to_string(),
                    description: "Commit message".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: true,
                },
                ToolParam {
                    name: "files".to_string(),
                    description: "Optional: comma-separated file paths to stage (default: stage all)".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: false,
                },
            ],
            trust_tier: ToolTrustTier::LocalMutating,
        }
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        let root = self.policy.workspace_root.clone();
        let message = match arg_str(call, "message") {
            Some(m) => m,
            None => {
                return make_result(
                    call,
                    false,
                    serde_json::json!({"error": "missing message"}),
                    "Commit failed".to_string(),
                )
            }
        };

        // Stage files
        let stage_result = if let Some(files) = arg_str(call, "files") {
            let file_args: Vec<&str> = files.split(',').map(|s| s.trim()).collect();
            let mut args = vec!["add"];
            args.extend(file_args);
            run_git(&args, &root).await
        } else {
            run_git(&["add", "-A"], &root).await
        };

        if let Err(e) = stage_result {
            return make_result(
                call,
                false,
                serde_json::json!({"error": e}),
                "Stage failed".to_string(),
            );
        }

        // Commit
        match run_git(&["commit", "-m", message], &root).await {
            Ok(output) => {
                // Get the commit hash
                let hash = run_git(&["rev-parse", "--short", "HEAD"], &root)
                    .await
                    .unwrap_or_else(|_| "unknown".to_string());
                let hash = hash.trim().to_string();
                make_result(
                    call,
                    true,
                    serde_json::json!({
                        "commit_hash": hash,
                        "message": message,
                        "output": output.trim(),
                    }),
                    format!(
                        "Committed: {} ({})",
                        message.lines().next().unwrap_or(""),
                        hash
                    ),
                )
            }
            Err(e) => make_result(
                call,
                false,
                serde_json::json!({"error": e}),
                "Commit failed".to_string(),
            ),
        }
    }
}

// ─────────────────────────────────────────────────── GitLogTool ──────────────

/// Shows recent commit history of the workspace repository. Trusted at
/// `Observational`.
pub struct GitLogTool {
    policy: Arc<crate::paths::PathPolicy>,
}
impl GitLogTool {
    /// Creates a new [`GitLogTool`].
    pub fn new() -> Self {
        Self::with_policy(Arc::new(crate::paths::PathPolicy::from_env()))
    }

    /// Creates a new [`GitLogTool`] with an explicitly supplied policy.
    ///
    /// Preferred for consistent configuration across tools (e.g. per-user
    /// workspace roots in chat mode); [`GitLogTool::new`] reads the
    /// environment instead.
    pub fn with_policy(policy: Arc<crate::paths::PathPolicy>) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl ToolExecutor for GitLogTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "git_log".to_string(),
            description: "Show recent git commit history. Returns commit hashes, authors, dates, and messages.".to_string(),
            parameters: vec![
                ToolParam {
                    name: "count".to_string(),
                    description: "Number of commits to show (default 10)".to_string(),
                    param_type: "integer".to_string(),
                    enum_values: None,
                    required: false,
                },
                ToolParam {
                    name: "file".to_string(),
                    description: "Optional: show only commits that modified this file".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: false,
                },
            ],
            trust_tier: ToolTrustTier::Observational,
        }
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        let root = self.policy.workspace_root.clone();
        let count = call.arg_u64("count").unwrap_or(10) as usize;
        let count_str = count.to_string();

        let mut args = vec!["log", "--oneline", "-n", &count_str];
        if let Some(file) = arg_str(call, "file") {
            args.push("--");
            args.push(file);
        }

        match run_git(&args, &root).await {
            Ok(output) => {
                let commits: Vec<&str> = output.lines().collect();
                make_result(
                    call,
                    true,
                    serde_json::json!({
                        "commits": commits.iter().map(|line| {
                            let parts: Vec<&str> = line.splitn(2, ' ').collect();
                            serde_json::json!({
                                "hash": parts.first().unwrap_or(&""),
                                "message": parts.get(1).unwrap_or(&""),
                            })
                        }).collect::<Vec<_>>(),
                        "count": commits.len(),
                    }),
                    format!("{} commits", commits.len()),
                )
            }
            Err(e) => make_result(
                call,
                false,
                serde_json::json!({"error": e}),
                "Git log failed".to_string(),
            ),
        }
    }
}

// ─────────────────────────────────────────────────── GitBranchTool ───────────

/// Lists, creates, or switches git branches in the workspace repository.
/// Trusted at `LocalMutating`.
pub struct GitBranchTool {
    policy: Arc<crate::paths::PathPolicy>,
}
impl GitBranchTool {
    /// Creates a new [`GitBranchTool`].
    pub fn new() -> Self {
        Self::with_policy(Arc::new(crate::paths::PathPolicy::from_env()))
    }

    /// Creates a new [`GitBranchTool`] with an explicitly supplied policy.
    ///
    /// Preferred for consistent configuration across tools (e.g. per-user
    /// workspace roots in chat mode); [`GitBranchTool::new`] reads the
    /// environment instead.
    pub fn with_policy(policy: Arc<crate::paths::PathPolicy>) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl ToolExecutor for GitBranchTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "git_branch".to_string(),
            description: "List git branches or create/switch to a branch.".to_string(),
            parameters: vec![
                ToolParam {
                    name: "action".to_string(),
                    description: "Action: 'list' (default), 'create', or 'switch'".to_string(),
                    param_type: "string".to_string(),
                    enum_values: Some(vec![
                        "list".to_string(),
                        "create".to_string(),
                        "switch".to_string(),
                    ]),
                    required: false,
                },
                ToolParam {
                    name: "name".to_string(),
                    description: "Branch name (required for create/switch)".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: false,
                },
            ],
            trust_tier: ToolTrustTier::LocalMutating,
        }
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        let root = self.policy.workspace_root.clone();
        let action = arg_str(call, "action").unwrap_or("list");

        match action {
            "list" => match run_git(&["branch", "--list"], &root).await {
                Ok(output) => {
                    let branches: Vec<&str> =
                        output.lines().map(|l| l.trim_start_matches("* ")).collect();
                    make_result(
                        call,
                        true,
                        serde_json::json!({
                            "branches": branches,
                            "count": branches.len(),
                        }),
                        format!("{} branches", branches.len()),
                    )
                }
                Err(e) => make_result(
                    call,
                    false,
                    serde_json::json!({"error": e}),
                    "Branch list failed".to_string(),
                ),
            },
            "create" | "switch" => {
                let name = match arg_str(call, "name") {
                    Some(n) => n,
                    None => {
                        return make_result(
                            call,
                            false,
                            serde_json::json!({"error": "missing branch name"}),
                            "Branch operation failed".to_string(),
                        )
                    }
                };
                let args = if action == "create" {
                    vec!["checkout", "-b", name]
                } else {
                    vec!["checkout", name]
                };
                match run_git(&args, &root).await {
                    Ok(output) => make_result(
                        call,
                        true,
                        serde_json::json!({
                            "branch": name,
                            "action": action,
                            "output": output.trim(),
                        }),
                        format!("{} branch: {}", action, name),
                    ),
                    Err(e) => make_result(
                        call,
                        false,
                        serde_json::json!({"error": e}),
                        format!("Branch {} failed", action),
                    ),
                }
            }
            _ => make_result(
                call,
                false,
                serde_json::json!({"error": "unknown action"}),
                "Invalid action".to_string(),
            ),
        }
    }
}

// ─────────────────────────────────────────────────── GitBlameTool ────────────

/// Shows per-line authorship (git blame) for a file in the workspace
/// repository. Trusted at `Observational`.
pub struct GitBlameTool {
    policy: Arc<crate::paths::PathPolicy>,
}
impl GitBlameTool {
    /// Creates a new [`GitBlameTool`].
    pub fn new() -> Self {
        Self::with_policy(Arc::new(crate::paths::PathPolicy::from_env()))
    }

    /// Creates a new [`GitBlameTool`] with an explicitly supplied policy.
    ///
    /// Preferred for consistent configuration across tools (e.g. per-user
    /// workspace roots in chat mode); [`GitBlameTool::new`] reads the
    /// environment instead.
    pub fn with_policy(policy: Arc<crate::paths::PathPolicy>) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl ToolExecutor for GitBlameTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "git_blame".to_string(),
            description: "Show git blame for a file — who changed each line and when. Useful for understanding code ownership.".to_string(),
            parameters: vec![
                ToolParam {
                    name: "file".to_string(),
                    description: "File path to blame".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: true,
                },
                ToolParam {
                    name: "line_range".to_string(),
                    description: "Optional: line range as 'start,end' (e.g. '10,50')".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: false,
                },
            ],
            trust_tier: ToolTrustTier::Observational,
        }
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        let root = self.policy.workspace_root.clone();
        let file = match arg_str(call, "file") {
            Some(f) => f,
            None => {
                return make_result(
                    call,
                    false,
                    serde_json::json!({"error": "missing file"}),
                    "Blame failed".to_string(),
                )
            }
        };

        let mut args = vec!["blame", "--porcelain", file];
        let line_range_str;
        if let Some(range) = arg_str(call, "line_range") {
            line_range_str = format!("-L{}", range);
            args.push(&line_range_str);
        }

        match run_git(&args, &root).await {
            Ok(output) => {
                let lines: Vec<&str> = output.lines().collect();
                make_result(
                    call,
                    true,
                    serde_json::json!({
                        "file": file,
                        "blame_output": output,
                        "lines": lines.len(),
                    }),
                    format!("Blame for {} ({} lines)", file, lines.len()),
                )
            }
            Err(e) => make_result(
                call,
                false,
                serde_json::json!({"error": e}),
                "Git blame failed".to_string(),
            ),
        }
    }
}
