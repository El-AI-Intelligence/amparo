//! Chat config — the tenant directory for the chat face.
//!
//! [`ChatConfig`] decides who may start tasks in chat: one
//! `platform:user_id` profile per tenant, each with an optional trust
//! ceiling (the per-user `--trust-ceiling`) and an optional workspace
//! subpath. It loads from a TOML file at startup, so the operator's config
//! file — not a compile flag — decides who gets a chat face and how far
//! they can go.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use amparo_tools::registry::ToolTrustTier;
use serde::Deserialize;
use thiserror::Error;

/// The M8 swarm knobs for one tenant: how many sub-agents the parent may
/// spawn, and whether the `schedule` tool is registered.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwarmProfile {
    /// Swarm budget: at most this many sub-agents per task; `0` turns
    /// `spawn_agent` off. Absent → 4 (the CLI default).
    #[serde(default = "default_max_sub_agents")]
    pub max_sub_agents: usize,
    /// Register the `schedule` tool (M8 W5): the tenant may persist
    /// tasks that re-enter the gate chain when they fire. Absent → false.
    #[serde(default)]
    pub schedule: bool,
}

/// The default swarm budget — the same 4 as the CLI's `--max-sub-agents`.
fn default_max_sub_agents() -> usize {
    4
}

impl Default for SwarmProfile {
    /// The driver default: budget 4, no schedule.
    fn default() -> Self {
        Self {
            max_sub_agents: 4,
            schedule: false,
        }
    }
}

/// One tenant's profile in a [`ChatConfig`] file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserProfile {
    /// Per-user trust ceiling; absent → the driver's default (the
    /// `--trust-ceiling` flag).
    #[serde(default)]
    pub trust_ceiling: Option<ToolTrustTier>,
    /// Workspace subpath relative to the workspace root; absent →
    /// `users/<platform>-<user_id>`. Absolute paths and paths containing
    /// `..` are rejected at load.
    #[serde(default)]
    pub workspace: Option<PathBuf>,
    /// Privacy-ledger quota in bytes for this tenant's ledger file;
    /// absent → unbounded. Zero is rejected at load — `None` is how a
    /// profile says "unbounded".
    #[serde(default)]
    pub ledger_max_bytes: Option<u64>,
    /// The M8 swarm knobs: sub-agent budget and the `schedule` tool.
    /// Absent → the driver's default (budget 4, no schedule).
    #[serde(default)]
    pub swarm: Option<SwarmProfile>,
}

/// The parsed chat config: the tenant directory — the only users who may
/// start tasks.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatConfig {
    /// Tenant directory, keyed `"platform:user_id"`.
    #[serde(default)]
    pub users: BTreeMap<String, UserProfile>,
}

impl ChatConfig {
    /// Loads and validates `path`: reads the file, parses TOML, then
    /// validates every tenant key (`platform:user_id` — a `:` with both
    /// parts non-empty) and every profile workspace (relative, no `..`).
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let config: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        for (key, profile) in &config.users {
            let (platform, user_id) = key.split_once(':').ok_or_else(|| ConfigError::Key {
                path: path.to_path_buf(),
                key: key.clone(),
            })?;
            if platform.is_empty() || user_id.is_empty() {
                return Err(ConfigError::Key {
                    path: path.to_path_buf(),
                    key: key.clone(),
                });
            }
            if let Some(workspace) = &profile.workspace {
                if workspace.is_absolute()
                    || workspace
                        .components()
                        .any(|c| matches!(c, Component::ParentDir))
                {
                    return Err(ConfigError::WorkspacePath {
                        path: path.to_path_buf(),
                        key: key.clone(),
                        value: workspace.clone(),
                    });
                }
            }
            if profile.ledger_max_bytes == Some(0) {
                return Err(ConfigError::Quota {
                    path: path.to_path_buf(),
                    key: key.clone(),
                });
            }
        }
        Ok(config)
    }

    /// True when there are no tenants — startup should warn rather than
    /// run mute.
    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
    }
}

/// Why a chat config failed to load or validate.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The config file could not be read.
    #[error("chat config {path}: {source}")]
    Io {
        /// The path that could not be read.
        path: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
    /// The config file is not valid TOML, or does not match the schema.
    #[error("chat config {path}: {source}")]
    Parse {
        /// The path whose content failed to parse.
        path: PathBuf,
        /// The underlying TOML/serde failure.
        #[source]
        source: toml::de::Error,
    },
    /// A tenant key is not `platform:user_id`.
    #[error("chat config {path}: tenant key {key:?} is not \"platform:user_id\"")]
    Key {
        /// The path that failed validation.
        path: PathBuf,
        /// The offending tenant key.
        key: String,
    },
    /// A profile workspace is absolute or escapes the workspace root.
    #[error(
        "chat config {path}: tenant {key}: workspace must be a relative path without \"..\" (got {value:?})"
    )]
    WorkspacePath {
        /// The path that failed validation.
        path: PathBuf,
        /// The offending tenant's key.
        key: String,
        /// The workspace value that was rejected.
        value: PathBuf,
    },
    /// A profile ledger quota is zero — a quota must bound the file.
    #[error("chat config {path}: tenant {key}: ledger_max_bytes must be positive (use no key for unbounded)")]
    Quota {
        /// The path that failed validation.
        path: PathBuf,
        /// The offending tenant's key.
        key: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes `body` to a uniquely named temp file outside the repo tree
    /// and returns its path; the caller removes the file when done.
    fn write_cfg(name: &str, body: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("amparo-chat-config-{name}.toml"));
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn happy_path_two_sections_one_all_defaults() {
        let path = write_cfg(
            "happy_path",
            "[users.\"telegram:111\"]\n[users.\"discord:222\"]\ntrust_ceiling = \"external_effector\"\n",
        );
        let config = ChatConfig::load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(config.users.len(), 2);
        assert_eq!(config.users["telegram:111"].trust_ceiling, None);
        assert_eq!(config.users["telegram:111"].workspace, None);
        assert_eq!(
            config.users["discord:222"].trust_ceiling,
            Some(ToolTrustTier::ExternalEffector)
        );
        assert_eq!(config.users["discord:222"].workspace, None);
    }

    #[test]
    fn empty_users_table_is_empty_config() {
        let path = write_cfg("empty_users", "[users]\n");
        let config = ChatConfig::load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(config.is_empty());
    }

    #[test]
    fn whitespace_only_file_parses_empty() {
        let path = write_cfg("whitespace_only", "   \n\t\n  ");
        let config = ChatConfig::load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(config.is_empty());
    }

    #[test]
    fn unknown_field_is_a_parse_error() {
        let path = write_cfg(
            "unknown_field",
            "[users.\"telegram:111\"]\nno_such_field = 1\n",
        );
        let err = ChatConfig::load(&path).unwrap_err();
        std::fs::remove_file(&path).unwrap();
        match &err {
            ConfigError::Parse { .. } => {}
            other => panic!("expected Parse, got {other:?}"),
        }
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[test]
    fn unknown_top_level_table_is_a_parse_error() {
        let path = write_cfg("unknown_top_level", "[wat]\n");
        let err = ChatConfig::load(&path).unwrap_err();
        std::fs::remove_file(&path).unwrap();
        match &err {
            ConfigError::Parse { .. } => {}
            other => panic!("expected Parse, got {other:?}"),
        }
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[test]
    fn bogus_tier_is_a_parse_error() {
        let path = write_cfg(
            "bogus_tier",
            "[users.\"telegram:111\"]\ntrust_ceiling = \"bogus\"\n",
        );
        let err = ChatConfig::load(&path).unwrap_err();
        std::fs::remove_file(&path).unwrap();
        match &err {
            ConfigError::Parse { .. } => {}
            other => panic!("expected Parse, got {other:?}"),
        }
        assert!(err.to_string().contains("unknown variant"), "{err}");
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    /// An absolute path that must trip the workspace rule on every
    /// platform. `"/etc/passwd"` is only absolute on Unix — on Windows it
    /// is drive-relative and slips through as a plain relative name.
    /// Forward slashes keep the TOML fixture free of `\` escapes while
    /// still parsing as an absolute Windows path (`C:/…`).
    fn absolute_escape_path() -> String {
        #[cfg(unix)]
        {
            "/etc/passwd".to_string()
        }
        #[cfg(not(unix))]
        {
            "C:/Windows/System32/notepad.exe".to_string()
        }
    }

    #[test]
    fn absolute_workspace_is_rejected() {
        let escape = absolute_escape_path();
        let body = format!("[users.\"telegram:111\"]\nworkspace = \"{escape}\"\n");
        let path = write_cfg("absolute_ws", &body);
        let err = ChatConfig::load(&path).unwrap_err();
        std::fs::remove_file(&path).unwrap();
        match &err {
            ConfigError::WorkspacePath { value, .. } => {
                assert_eq!(value, &PathBuf::from(&escape))
            }
            other => panic!("expected WorkspacePath, got {other:?}"),
        }
    }

    #[test]
    fn dotdot_workspace_is_rejected() {
        let path = write_cfg(
            "dotdot_ws",
            "[users.\"telegram:111\"]\nworkspace = \"../escape\"\n",
        );
        let err = ChatConfig::load(&path).unwrap_err();
        std::fs::remove_file(&path).unwrap();
        match &err {
            ConfigError::WorkspacePath { value, .. } => {
                assert_eq!(value, &PathBuf::from("../escape"))
            }
            other => panic!("expected WorkspacePath, got {other:?}"),
        }
    }

    #[test]
    fn relative_workspace_is_accepted() {
        let path = write_cfg(
            "relative_ws",
            "[users.\"telegram:111\"]\nworkspace = \"team-a\"\n",
        );
        let config = ChatConfig::load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            config.users["telegram:111"].workspace,
            Some(PathBuf::from("team-a"))
        );
    }

    #[test]
    fn key_without_colon_is_rejected() {
        let path = write_cfg("no_colon", "[users.\"telegram111\"]\n");
        let err = ChatConfig::load(&path).unwrap_err();
        std::fs::remove_file(&path).unwrap();
        match &err {
            ConfigError::Key { key, .. } => assert_eq!(key, "telegram111"),
            other => panic!("expected Key, got {other:?}"),
        }
    }

    #[test]
    fn key_with_empty_platform_is_rejected() {
        for body in ["[users.\":111\"]\n", "[users.\"telegram:\"]\n"] {
            let path = write_cfg("empty_platform", body);
            let err = ChatConfig::load(&path).unwrap_err();
            std::fs::remove_file(&path).unwrap();
            match &err {
                ConfigError::Key { .. } => {}
                other => panic!("expected Key, got {other:?}"),
            }
        }
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let path = std::env::temp_dir().join("amparo-chat-config-definitely-not-here.toml");
        let err = ChatConfig::load(&path).unwrap_err();
        match &err {
            ConfigError::Io { .. } => {}
            other => panic!("expected Io, got {other:?}"),
        }
    }

    #[test]
    fn ledger_quota_parses_and_defaults_to_unbounded() {
        let path = write_cfg(
            "ledger_quota",
            "[users.\"telegram:111\"]\nledger_max_bytes = 4096\n[users.\"discord:222\"]\n",
        );
        let config = ChatConfig::load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(config.users["telegram:111"].ledger_max_bytes, Some(4096));
        assert_eq!(config.users["discord:222"].ledger_max_bytes, None);
    }

    #[test]
    fn zero_ledger_quota_is_rejected() {
        let path = write_cfg(
            "zero_quota",
            "[users.\"telegram:111\"]\nledger_max_bytes = 0\n",
        );
        let err = ChatConfig::load(&path).unwrap_err();
        std::fs::remove_file(&path).unwrap();
        match &err {
            ConfigError::Quota { key, .. } => assert_eq!(key, "telegram:111"),
            other => panic!("expected Quota, got {other:?}"),
        }
    }

    #[test]
    fn swarm_profile_parses_with_defaults_and_is_absent_by_default() {
        let path = write_cfg(
            "swarm",
            "[users.\"telegram:111\"]\n[users.\"telegram:111\".swarm]\nmax_sub_agents = 2\nschedule = true\n[users.\"discord:222\"]\n[users.\"discord:222\".swarm]\n",
        );
        let config = ChatConfig::load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        // An explicit profile carries both knobs…
        let explicit = config.users["telegram:111"].swarm.as_ref().unwrap();
        assert_eq!(explicit.max_sub_agents, 2);
        assert!(explicit.schedule);
        // …a bare `[swarm]` table defaults the budget to 4 and schedule
        // to false…
        let bare = config.users["discord:222"].swarm.as_ref().unwrap();
        assert_eq!(bare.max_sub_agents, 4);
        assert!(!bare.schedule);
        // …and a tenant with no table has no swarm profile at all (the
        // driver applies its default).
        assert!(SwarmProfile::default().max_sub_agents == 4);
    }

    #[test]
    fn swarm_profile_rejects_unknown_keys() {
        let path = write_cfg(
            "swarm_unknown",
            "[users.\"telegram:111\".swarm]\nmax_sub_agents = 2\nnope = true\n",
        );
        let err = ChatConfig::load(&path).unwrap_err();
        std::fs::remove_file(&path).unwrap();
        match &err {
            ConfigError::Parse { .. } => {}
            other => panic!("expected Parse, got {other:?}"),
        }
    }
}
