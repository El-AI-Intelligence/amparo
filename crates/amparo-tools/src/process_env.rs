//! Secret-free subprocess environments (audit 2026-08-31 MED-3).
//!
//! `git` and `run_build` spawn child processes that run arbitrary code
//! (git hooks, build scripts). [`secret_free_env`] replaces the
//! inherited environment with the process env minus every `AMPARO_*`
//! key, so policy keys and other Amparo secrets never reach them. The
//! strip is surgical: toolchain variables (`PATH`, `CARGO`,
//! `RUSTUP_*`, …) are retained — an `env_clear` alone would break
//! cargo/rustup resolution.

use tokio::process::Command;

/// Configure `cmd` to inherit the current process environment with
/// every `AMPARO_*` key removed.
pub fn secret_free_env(cmd: &mut Command) {
    cmd.env_clear().envs(strip_amparo_keys(std::env::vars()));
}

/// Keep every var whose key does not start with `AMPARO_`.
fn strip_amparo_keys(vars: impl IntoIterator<Item = (String, String)>) -> Vec<(String, String)> {
    vars.into_iter()
        .filter(|(key, _)| !key.starts_with("AMPARO_"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_amparo_keys_and_keeps_toolchain_vars() {
        let kept = strip_amparo_keys(vec![
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("CARGO".to_string(), "/root/.cargo/bin/cargo".to_string()),
            ("RUSTUP_HOME".to_string(), "/root/.rustup".to_string()),
            ("AMPARO_POLICY_KEY".to_string(), "gk_secret".to_string()),
            ("AMPARO_ENGRAM_KEY".to_string(), "k".to_string()),
            ("amparo_lowercase".to_string(), "kept".to_string()),
        ]);
        let keys: Vec<&str> = kept.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(kept.len(), 4, "the AMPARO_* key set is stripped");
        assert!(keys.iter().all(|key| !key.starts_with("AMPARO_")));
        assert!(keys.contains(&"PATH"));
        assert!(keys.contains(&"CARGO"));
        assert!(keys.contains(&"RUSTUP_HOME"));
        assert!(keys.contains(&"amparo_lowercase"));
    }
}
