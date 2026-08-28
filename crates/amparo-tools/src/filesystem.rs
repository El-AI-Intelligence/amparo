// Originally part of Axiom-OS (MIT, Copyright (c) Pixel Phantom AI).
// Ported to Amparo and relicensed Apache-2.0 — see the repository NOTICE.

//! Filesystem tools — sandboxed read/write/list/edit/patch (Layer 3.2).
//!
//! All filesystem access is restricted to paths inside the workspace root
//! (defaults to ~/amparo-workspace). The sandbox policy is enforced by
//! `PathPolicy::resolve_workspace_path()`, which rejects path traversal
//! escapes. Write operations additionally check `is_path_allowed(path, write=true)`.
//!
//! The workspace root is configurable via `AMPARO_WORKSPACE` env var or
//! programmatically through `PathPolicy`.

use super::{ToolCall, ToolExecutor, ToolParam, ToolResult, ToolSchema, ToolTrustTier};
use async_trait::async_trait;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

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

// ── Helper: resolve a user path against the sandbox policy ─────────────────

fn resolve_path(
    policy: &crate::paths::PathPolicy,
    user_path: &str,
    write: bool,
) -> Result<PathBuf, String> {
    let resolved = policy.resolve_workspace_path(user_path)?;
    if write && !policy.is_path_allowed(&resolved, true) {
        return Err(format!(
            "Path '{}' is outside the writable workspace. Access denied.",
            user_path
        ));
    }
    Ok(resolved)
}

// ─────────────────────────────────────────────────── ReadFileTool ────────────

/// Reads a file inside the workspace, enforcing the path-confinement checks in
/// [`crate::paths::PathPolicy`]. Trusted at `Observational`.
pub struct ReadFileTool {
    policy: Arc<crate::paths::PathPolicy>,
}

impl ReadFileTool {
    /// Creates a new [`ReadFileTool`] with the path policy loaded from
    /// environment variables.
    pub fn new() -> Self {
        Self::with_policy(Arc::new(crate::paths::PathPolicy::from_env()))
    }

    /// Creates a new [`ReadFileTool`] with an explicitly supplied policy.
    ///
    /// Preferred for consistent configuration across tools (e.g. per-user
    /// workspace roots in chat mode); [`ReadFileTool::new`] reads the
    /// environment instead.
    pub fn with_policy(policy: Arc<crate::paths::PathPolicy>) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl ToolExecutor for ReadFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "read_file".to_string(),
            description: "Read the contents of a file in the Amparo workspace. The workspace is ~/amparo-workspace. Provide a path relative to the workspace root.".to_string(),
            parameters: vec![
                ToolParam {
                    name: "path".to_string(),
                    description: "Path to the file, relative to workspace root (e.g. 'notes/ideas.md')".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: true,
                },
                ToolParam {
                    name: "max_chars".to_string(),
                    description: "Maximum characters to return (default 8000)".to_string(),
                    param_type: "integer".to_string(),
                    enum_values: None,
                    required: false,
                },
            ],
            trust_tier: ToolTrustTier::Observational,
        }
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        let path = match arg_str(call, "path") {
            Some(p) => p,
            None => return make_result(call, false, serde_json::json!({"error": "missing path"}), "Read failed".to_string()),
        };
        let max_chars = call.arg_u64("max_chars").unwrap_or(8000).min(40000) as usize;

        match resolve_path(&self.policy, path, false) {
            Err(e) => make_result(call, false, serde_json::json!({"error": e}), format!("Access denied: {}", e)),
            Ok(abs_path) => {
                match tokio::fs::read_to_string(&abs_path).await {
                    Ok(content) => {
                        let truncated = if content.len() > max_chars {
                            format!("{}... [truncated]", &content[..max_chars])
                        } else { content.clone() };
                        make_result(call, true, serde_json::json!({
                            "path": path,
                            "content": truncated,
                            "size_bytes": content.len(),
                            "truncated": content.len() > max_chars,
                        }), format!("Read {} bytes from {}", content.len().min(max_chars), path))
                    }
                    Err(e) => make_result(call, false, serde_json::json!({"error": e.to_string(), "path": path}), format!("Read failed: {}", e)),
                }
            }
        }
    }
}

// ──────────────────────────────────────────────────── WriteFileTool ───────────

/// Writes or appends a file inside the workspace, creating parent directories
/// as needed; paths outside the writable workspace are denied. Trusted at
/// `LocalMutating`.
pub struct WriteFileTool {
    policy: Arc<crate::paths::PathPolicy>,
}

impl WriteFileTool {
    /// Creates a new [`WriteFileTool`] with the path policy loaded from
    /// environment variables.
    pub fn new() -> Self {
        Self::with_policy(Arc::new(crate::paths::PathPolicy::from_env()))
    }

    /// Creates a new [`WriteFileTool`] with an explicitly supplied policy.
    ///
    /// Preferred for consistent configuration across tools (e.g. per-user
    /// workspace roots in chat mode); [`WriteFileTool::new`] reads the
    /// environment instead.
    pub fn with_policy(policy: Arc<crate::paths::PathPolicy>) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl ToolExecutor for WriteFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "write_file".to_string(),
            description: "Write or append content to a file in the Amparo workspace. Parent directories are created automatically. The workspace is ~/amparo-workspace.".to_string(),
            parameters: vec![
                ToolParam {
                    name: "path".to_string(),
                    description: "Destination path relative to workspace root".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: true,
                },
                ToolParam {
                    name: "content".to_string(),
                    description: "Content to write".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: true,
                },
                ToolParam {
                    name: "mode".to_string(),
                    description: "Write mode: 'overwrite' replaces the file, 'append' adds to end (default: overwrite)".to_string(),
                    param_type: "string".to_string(),
                    enum_values: Some(vec!["overwrite".to_string(), "append".to_string()]),
                    required: false,
                },
            ],
            trust_tier: ToolTrustTier::LocalMutating,
        }
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        let path = match arg_str(call, "path") {
            Some(p) => p,
            None => return make_result(call, false, serde_json::json!({"error": "missing path"}), "Write failed".to_string()),
        };
        let content = match arg_str(call, "content") {
            Some(c) => c.to_string(),
            None => return make_result(call, false, serde_json::json!({"error": "missing content"}), "Write failed".to_string()),
        };
        let mode = arg_str(call, "mode").unwrap_or("overwrite");

        match resolve_path(&self.policy, path, true) {
            Err(e) => make_result(call, false, serde_json::json!({"error": e}), format!("Access denied: {}", e)),
            Ok(abs_path) => {
                if let Some(parent) = abs_path.parent() {
                    tokio::fs::create_dir_all(parent).await.ok();
                }
                let result = if mode == "append" {
                    use tokio::io::AsyncWriteExt;
                    let mut file = tokio::fs::OpenOptions::new()
                        .create(true).append(true).open(&abs_path).await;
                    match file {
                        Ok(ref mut f) => f.write_all(content.as_bytes()).await,
                        Err(e) => Err(e),
                    }
                } else {
                    tokio::fs::write(&abs_path, content.as_bytes()).await
                };

                match result {
                    Ok(_) => make_result(call, true, serde_json::json!({
                        "path": path,
                        "bytes_written": content.len(),
                        "mode": mode,
                    }), format!("Wrote {} bytes to {}", content.len(), path)),
                    Err(e) => make_result(call, false, serde_json::json!({"error": e.to_string(), "path": path}), format!("Write failed: {}", e)),
                }
            }
        }
    }
}

// ─────────────────────────────────────────────────── ListDirTool ────────────

/// Lists the contents of a directory inside the workspace sandbox. Trusted at
/// `Observational`.
pub struct ListDirTool {
    policy: Arc<crate::paths::PathPolicy>,
}

impl ListDirTool {
    /// Creates a new [`ListDirTool`] with the path policy loaded from
    /// environment variables.
    pub fn new() -> Self {
        Self::with_policy(Arc::new(crate::paths::PathPolicy::from_env()))
    }

    /// Creates a new [`ListDirTool`] with an explicitly supplied policy.
    ///
    /// Preferred for consistent configuration across tools (e.g. per-user
    /// workspace roots in chat mode); [`ListDirTool::new`] reads the
    /// environment instead.
    pub fn with_policy(policy: Arc<crate::paths::PathPolicy>) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl ToolExecutor for ListDirTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "list_dir".to_string(),
            description: "List the contents of a directory in the Amparo workspace. Returns file names, types, and sizes.".to_string(),
            parameters: vec![
                ToolParam {
                    name: "path".to_string(),
                    description: "Directory path relative to workspace root (use '.' for workspace root)".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: true,
                },
            ],
            trust_tier: ToolTrustTier::Observational,
        }
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        let path = arg_str(call, "path").unwrap_or(".");
        match resolve_path(&self.policy, path, false) {
            Err(e) => make_result(call, false, serde_json::json!({"error": e}), "Access denied".to_string()),
            Ok(abs_path) => {
                match tokio::fs::read_dir(&abs_path).await {
                    Ok(mut dir) => {
                        let mut entries = Vec::new();
                        while let Ok(Some(entry)) = dir.next_entry().await {
                            let name = entry.file_name().to_string_lossy().to_string();
                            let meta = entry.metadata().await;
                            let (is_dir, size) = match meta {
                                Ok(m) => (m.is_dir(), m.len()),
                                Err(_) => (false, 0),
                            };
                            entries.push(serde_json::json!({
                                "name": name,
                                "type": if is_dir { "directory" } else { "file" },
                                "size_bytes": size,
                            }));
                        }
                        make_result(call, true, serde_json::json!({
                            "path": path,
                            "entries": entries,
                            "count": entries.len(),
                        }), format!("{} items in {}", entries.len(), path))
                    }
                    Err(e) => make_result(call, false, serde_json::json!({"error": e.to_string()}), format!("List failed: {}", e)),
                }
            }
        }
    }
}

// ─────────────────────────────────────────────────── EditFileTool ────────────

/// Applies SEARCH/REPLACE edits to a file in the workspace, writing back with
/// a backup; fails closed when the search text is not found. Trusted at
/// `LocalMutating`.
pub struct EditFileTool {
    policy: Arc<crate::paths::PathPolicy>,
}

impl EditFileTool {
    /// Creates a new [`EditFileTool`] with the path policy loaded from
    /// environment variables.
    pub fn new() -> Self {
        Self::with_policy(Arc::new(crate::paths::PathPolicy::from_env()))
    }

    /// Creates a new [`EditFileTool`] with an explicitly supplied policy.
    ///
    /// Preferred for consistent configuration across tools (e.g. per-user
    /// workspace roots in chat mode); [`EditFileTool::new`] reads the
    /// environment instead.
    pub fn with_policy(policy: Arc<crate::paths::PathPolicy>) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl ToolExecutor for EditFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "edit_file".to_string(),
            description: "Edit a file surgically using SEARCH/REPLACE blocks. Provide the exact text to find and the replacement text. Multiple blocks can be applied in order. The file is read, patched, and written back atomically with a backup.".to_string(),
            parameters: vec![
                ToolParam {
                    name: "path".to_string(),
                    description: "File path relative to workspace root".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: true,
                },
                ToolParam {
                    name: "old_text".to_string(),
                    description: "Exact text to search for (must match character-for-character including whitespace)".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: true,
                },
                ToolParam {
                    name: "new_text".to_string(),
                    description: "Replacement text".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: true,
                },
            ],
            trust_tier: ToolTrustTier::LocalMutating,
        }
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        let path = match arg_str(call, "path") {
            Some(p) => p,
            None => return make_result(call, false, serde_json::json!({"error": "missing path"}), "Edit failed".to_string()),
        };
        let old_text = match arg_str(call, "old_text") {
            Some(t) => t,
            None => return make_result(call, false, serde_json::json!({"error": "missing old_text"}), "Edit failed".to_string()),
        };
        let new_text = match arg_str(call, "new_text") {
            Some(t) => t,
            None => return make_result(call, false, serde_json::json!({"error": "missing new_text"}), "Edit failed".to_string()),
        };

        let abs_path = match resolve_path(&self.policy, path, true) {
            Err(e) => return make_result(call, false, serde_json::json!({"error": e}), format!("Access denied: {}", e)),
            Ok(p) => p,
        };

        let content = match tokio::fs::read_to_string(&abs_path).await {
            Ok(c) => c,
            Err(e) => return make_result(call, false, serde_json::json!({"error": e.to_string()}), format!("Read failed: {}", e)),
        };

        let match_count = content.matches(old_text).count();
        if match_count == 0 {
            return make_result(call, false, serde_json::json!({
                "error": "SEARCH text not found in file",
                "path": path,
                "search_preview": old_text.chars().take(80).collect::<String>(),
            }), "Search text not found".to_string());
        }

        let new_content = content.replace(old_text, new_text);

        // Create backup
        let backup_path = format!("{}.bak", abs_path.display());
        tokio::fs::write(&backup_path, &content).await.ok();

        match tokio::fs::write(&abs_path, &new_content).await {
            Ok(_) => {
                let lines_changed = new_text.lines().count() as i64 - old_text.lines().count() as i64;
                make_result(call, true, serde_json::json!({
                    "path": path,
                    "replacements": match_count,
                    "lines_changed": lines_changed,
                    "size_before": content.len(),
                    "size_after": new_content.len(),
                    "backup": backup_path,
                }), format!("Edited {} — {} replacement(s)", path, match_count))
            }
            Err(e) => make_result(call, false, serde_json::json!({"error": e.to_string()}), format!("Write failed: {}", e)),
        }
    }
}

// ─────────────────────────────────────────────────── PatchFileTool ───────────

/// Applies a unified diff to a file in the workspace, writing back with a
/// backup. Trusted at `LocalMutating`.
pub struct PatchFileTool {
    policy: Arc<crate::paths::PathPolicy>,
}

impl PatchFileTool {
    /// Creates a new [`PatchFileTool`] with the path policy loaded from
    /// environment variables.
    pub fn new() -> Self {
        Self::with_policy(Arc::new(crate::paths::PathPolicy::from_env()))
    }

    /// Creates a new [`PatchFileTool`] with an explicitly supplied policy.
    ///
    /// Preferred for consistent configuration across tools (e.g. per-user
    /// workspace roots in chat mode); [`PatchFileTool::new`] reads the
    /// environment instead.
    pub fn with_policy(policy: Arc<crate::paths::PathPolicy>) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl ToolExecutor for PatchFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "patch_file".to_string(),
            description: "Apply a unified diff patch to a file. The patch should use standard unified diff format with ---/+++ headers and @@ hunk markers. Applies the patch and returns a verification summary.".to_string(),
            parameters: vec![
                ToolParam {
                    name: "path".to_string(),
                    description: "File path relative to workspace root".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: true,
                },
                ToolParam {
                    name: "patch".to_string(),
                    description: "Unified diff content (standard patch format)".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: true,
                },
            ],
            trust_tier: ToolTrustTier::LocalMutating,
        }
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        let path = match arg_str(call, "path") {
            Some(p) => p,
            None => return make_result(call, false, serde_json::json!({"error": "missing path"}), "Patch failed".to_string()),
        };
        let patch_text = match arg_str(call, "patch") {
            Some(p) => p,
            None => return make_result(call, false, serde_json::json!({"error": "missing patch"}), "Patch failed".to_string()),
        };

        let abs_path = match resolve_path(&self.policy, path, true) {
            Err(e) => return make_result(call, false, serde_json::json!({"error": e}), format!("Access denied: {}", e)),
            Ok(p) => p,
        };

        let content = match tokio::fs::read_to_string(&abs_path).await {
            Ok(c) => c,
            Err(e) => return make_result(call, false, serde_json::json!({"error": e.to_string()}), format!("Read failed: {}", e)),
        };

        match apply_unified_patch(&content, patch_text) {
            Ok(new_content) => {
                let backup_path = format!("{}.bak", abs_path.display());
                tokio::fs::write(&backup_path, &content).await.ok();

                match tokio::fs::write(&abs_path, &new_content).await {
                    Ok(_) => make_result(call, true, serde_json::json!({
                        "path": path,
                        "size_before": content.len(),
                        "size_after": new_content.len(),
                        "backup": backup_path,
                        "lines_before": content.lines().count(),
                        "lines_after": new_content.lines().count(),
                    }), format!("Patched {} successfully", path)),
                    Err(e) => make_result(call, false, serde_json::json!({"error": e.to_string()}), format!("Write failed: {}", e)),
                }
            }
            Err(e) => make_result(call, false, serde_json::json!({"error": e, "path": path}), format!("Patch failed: {}", e)),
        }
    }
}

// ── Unified diff patcher ────────────────────────────────────────────────────

/// Apply a simple unified diff to source text.
fn apply_unified_patch(source: &str, patch: &str) -> std::result::Result<String, String> {
    let source_lines: Vec<&str> = source.lines().collect();
    let patch_lines: Vec<&str> = patch.lines().collect();

    let mut result = source_lines.clone();
    let mut offset: isize = 0;

    let mut i = 0;
    while i < patch_lines.len() {
        let line = patch_lines[i].trim_end();

        if line.starts_with("---") || line.starts_with("+++") {
            i += 1;
            continue;
        }

        if line.starts_with("@@") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 3 {
                return Err(format!("Invalid hunk header: {}", line));
            }

            let old_range = parts[1].trim_start_matches('-');
            let old_start: usize = old_range.split(',')
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1);

            i += 1;
            let mut hunk_lines: Vec<&str> = Vec::new();
            while i < patch_lines.len() && !patch_lines[i].trim_end().starts_with("@@") {
                let hline = patch_lines[i];
                if !hline.is_empty() {
                    hunk_lines.push(hline);
                }
                i += 1;
            }

            let mut old_text: Vec<&str> = Vec::new();
            let mut new_text: Vec<&str> = Vec::new();

            for &hline in &hunk_lines {
                if hline.starts_with('-') {
                    old_text.push(&hline[1..]);
                } else if hline.starts_with('+') {
                    new_text.push(&hline[1..]);
                } else if hline.starts_with(' ') {
                    let ctx = &hline[1..];
                    old_text.push(ctx);
                    new_text.push(ctx);
                }
            }

            let search_start = (old_start as isize + offset - 1).max(0) as usize;
            if let Some(pos) = find_subsequence(&result, &old_text, search_start) {
                result.splice(pos..pos + old_text.len(), new_text.iter().copied());
                offset += new_text.len() as isize - old_text.len() as isize;
            }
        } else {
            i += 1;
        }
    }

    let mut output = result.join("\n");
    if source.ends_with('\n') && !output.ends_with('\n') {
        output.push('\n');
    }
    Ok(output)
}

fn find_subsequence(source: &[&str], needle: &[&str], start: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(start.min(source.len()));
    }
    for pos in start..source.len() {
        if pos + needle.len() > source.len() {
            break;
        }
        if source[pos..pos + needle.len()].iter().zip(needle.iter()).all(|(a, b)| a == b) {
            return Some(pos);
        }
    }
    None
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_simple_patch() {
        let source = "line1\nline2\nline3\nline4\n";
        let patch = "@@ -1,4 +1,4 @@\n line1\n-line2\n+REPLACED\n line3\n line4\n";
        let result = apply_unified_patch(source, patch).unwrap();
        assert!(result.contains("REPLACED"));
        assert!(!result.contains("line2"));
    }

    #[test]
    fn test_apply_patch_with_addition() {
        let source = "line1\nline2\n";
        let patch = "@@ -1,2 +1,3 @@\n line1\n line2\n+line3\n";
        let result = apply_unified_patch(source, patch).unwrap();
        assert!(result.contains("line3"));
    }

    #[test]
    fn test_apply_patch_with_deletion() {
        let source = "line1\nline2\nline3\n";
        let patch = "@@ -1,3 +1,2 @@\n line1\n-line2\n line3\n";
        let result = apply_unified_patch(source, patch).unwrap();
        assert!(!result.contains("line2"));
    }

    #[test]
    fn test_find_subsequence() {
        let source = vec!["a", "b", "c", "d", "e"];
        assert_eq!(find_subsequence(&source, &["b", "c", "d"], 0), Some(1));
        assert_eq!(find_subsequence(&source, &["x"], 0), None);
    }

    #[tokio::test]
    async fn with_policy_roots_tool_at_the_given_root() {
        let root = std::env::temp_dir().join(format!("amparo-with-policy-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("seed.txt"), "injected content").unwrap();
        let policy = Arc::new(crate::paths::PathPolicy::from_root(root.clone()));

        let read = ReadFileTool::with_policy(Arc::clone(&policy));
        let call = ToolCall {
            id: "1".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "seed.txt"}),
        };
        let result = read.execute(&call).await;
        assert!(result.success, "output: {}", result.output);
        assert_eq!(result.output["content"], "injected content");

        let write = WriteFileTool::with_policy(Arc::clone(&policy));
        let call = ToolCall {
            id: "2".to_string(),
            name: "write_file".to_string(),
            arguments: serde_json::json!({"path": "sub/out.txt", "content": "hello"}),
        };
        let result = write.execute(&call).await;
        assert!(result.success, "output: {}", result.output);
        let written = std::fs::read_to_string(root.join("sub/out.txt")).unwrap();
        assert_eq!(written, "hello");
    }
}
