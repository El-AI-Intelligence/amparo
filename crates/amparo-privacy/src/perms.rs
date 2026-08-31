//! Owner-only workspace permissions (audit 2026-08-31 MED-6).
//!
//! Workspace state — sessions, the privacy ledger, the notebook, the
//! blackboard, the schedule — holds transcripts and provenance rows.
//! Every creation site calls [`owner_only`] so a local user on a shared
//! box can neither read the state nor forge rows: files get 0600,
//! directories get 0700. Unix-only by design: on other targets the call
//! is a documented no-op (Windows ACLs are out of scope for the
//! workspace model).

use std::io;
use std::path::Path;

/// Set owner-only permissions on `path`: 0600 for files, 0700 for
/// directories.
///
/// Call this at every creation site — right after a directory is
/// created, and after a file is opened but before any content is
/// written — so state never exists for an instant with group/other
/// read access. On non-Unix targets this is a no-op.
pub fn owner_only(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if path.is_dir() { 0o700 } else { 0o600 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("amparo-perms-{}-{}", std::process::id(), tag))
    }

    #[test]
    fn owner_only_returns_ok_on_any_target() {
        let dir = temp_path("ok");
        fs::create_dir_all(&dir).unwrap();
        owner_only(&dir).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn files_get_0600_and_dirs_get_0700() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_path("modes");
        fs::create_dir_all(&dir).unwrap();
        owner_only(&dir).unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700,
            "directory must be owner-only"
        );
        let file = dir.join("state.json");
        fs::write(&file, b"x").unwrap();
        owner_only(&file).unwrap();
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600,
            "file must be owner-only"
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
