//! The `amparo chat` entry point — platform selection, flag parsing, and the
//! shared wiring every adapter's serve loop is built from.
//!
//! [`parse_chat_flags`] follows the `amparo run` idiom: one positional
//! platform (`telegram` | `discord` | `slack`), then `--policy-url` /
//! `--allow-all` (mutually exclusive), `--auto-approve`,
//! `--trust-ceiling`, `--chat-config` and `--help`. [`build_driver`] is the
//! shared assembly the adapters use so Discord and Slack do not duplicate
//! it: the inference provider from the environment (fail-closed, with the
//! same README-pointer hint as `amparo run`), the policy match with
//! byte-identical strings to `amparo run`, the shared tool registry, and
//! the tenancy — a `--chat-config`/`AMPARO_CHAT_CONFIG` TOML tenant
//! directory (per-user workspaces and ceilings), or the
//! `AMPARO_CHAT_ALLOWLIST` user allowlist — absent or empty means nobody,
//! and the startup warning says so.
//!
//! Fail-closed everywhere: a missing bot token is exit 2, a missing
//! inference configuration is exit 1, a config file that fails to load is
//! exit 2, and an empty tenant directory or allowlist refuses every
//! message. [`dispatch`] turns a [`ChatServeError`] into the process exit
//! code it carries.

use crate::config::ChatConfig;
use crate::driver::{ChatDriver, PolicySource, Tenants};
use crate::notification::ChatNotificationTransport;
use crate::router::ApprovalRouter;
use crate::transport::ChatTransport;
use amparo_inference::InferenceConfig;
use amparo_notebook::{notebook_dir, JsonlStore, HOT_FILE};
use amparo_policy::{AllowAllPolicyEngine, DenyAllPolicyEngine};
use amparo_sandbox::EvalWasmTool;
use amparo_tools::{
    default_registry_with_memory, resolve_memory_backend, SendNotificationTool, ToolTrustTier,
};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

/// The `amparo chat` usage text — printed by `--help` and referenced by
/// flag errors.
pub const CHAT_USAGE: &str = "\
amparo chat — serve the agent over a messaging platform

USAGE:
  amparo chat <platform> [FLAGS]

PLATFORMS:
  telegram    long-poll the Telegram Bot API with inline-button approval
  discord     gateway websocket (intents, resume, message components)
  slack       Socket Mode (envelope acks, block-kit buttons)

FLAGS:
  --policy-url URL    wire a remote policy engine (deny-all default)
  --allow-all         run without policy checks (explicit opt-in)
  --auto-approve      approve escalated/external-effector calls without a human
  --trust-ceiling T   observational | local_mutating |
                      external_effector | system_control (default)
  --chat-config PATH  load the tenant directory from a TOML config file
  --growth            record PII-stripped run records (off by default)
  --no-growth         never record (overrides an earlier --growth)
  --help              print this help and exit

Deny-by-default: without --policy-url or --allow-all every tool call is
refused, and escalated/external-effector calls ask for approval unless
--auto-approve overrides. Without --chat-config, only the users listed in
AMPARO_CHAT_ALLOWLIST (comma-separated ids) may start tasks — absent or
empty means every message is refused. With a chat config (the flag wins
over the AMPARO_CHAT_CONFIG environment variable), the file is the tenant
directory: [users.\"platform:user_id\"] sections, each with an optional
per-user trust_ceiling and workspace subpath; AMPARO_CHAT_ALLOWLIST is
ignored while one is set. The bot token comes from AMPARO_CHAT_TELEGRAM_TOKEN
(Telegram), the workspace root from AMPARO_WORKSPACE, and the inference
endpoint from the AMPARO_INFERENCE_* environment surface — see the README
chat section.

--growth enables the lab notebook: every completed or failed task is
recorded as a PII-stripped JSON line at
<workspace>/.amparo/notebook/records.jsonl, tagged platform:user_id (task
text, tool-sequence hash, per-call gate log, verification, truncated
answer). Recording is off by default — growth never happens unless asked
for — and the last --growth/--no-growth wins.";

/// The messaging platform `amparo chat` serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// Telegram — `getUpdates` long polling with inline-button approval.
    Telegram,
    /// Discord — gateway websocket (adapter lands in the next commit).
    Discord,
    /// Slack — Socket Mode (adapter lands in the next commit).
    Slack,
}

impl Platform {
    /// Parse a platform name as given on the command line.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "telegram" => Some(Self::Telegram),
            "discord" => Some(Self::Discord),
            "slack" => Some(Self::Slack),
            _ => None,
        }
    }

    /// The platform's canonical name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Telegram => "telegram",
            Self::Discord => "discord",
            Self::Slack => "slack",
        }
    }
}

/// Parsed `amparo chat` flags.
#[derive(Debug, Clone)]
pub struct ChatFlags {
    /// The platform to serve.
    pub platform: Platform,
    /// Wire a remote policy engine (deny-all default without this).
    pub policy_url: Option<String>,
    /// Run without policy checks (explicit opt-in).
    pub allow_all: bool,
    /// Approve escalated/external-effector calls without asking.
    pub auto_approve: bool,
    /// The trust ceiling every task's agent runs with.
    pub trust_ceiling: ToolTrustTier,
    /// The tenant-directory config file (flag form of `AMPARO_CHAT_CONFIG`).
    pub chat_config: Option<PathBuf>,
    /// Record PII-stripped run records to the workspace notebook.
    pub growth: bool,
}

impl Default for ChatFlags {
    fn default() -> Self {
        Self {
            platform: Platform::Telegram,
            policy_url: None,
            allow_all: false,
            auto_approve: false,
            trust_ceiling: ToolTrustTier::SystemControl,
            chat_config: None,
            growth: false,
        }
    }
}

/// Outcome of [`parse_chat_flags`]: serve with these flags, print
/// [`CHAT_USAGE`] and exit 0, or print the message to stderr and exit 2.
#[derive(Debug)]
pub enum ParseChatResult {
    /// Serve the platform with these flags.
    Serve(ChatFlags),
    /// `--help` was requested.
    Help,
    /// A flag or platform problem — print the message and exit 2.
    Error(String),
}

/// A serve failure carrying the process exit code it maps to
/// (2 = configuration problem, 1 = serve/runtime failure).
#[derive(Debug, Clone)]
pub struct ChatServeError {
    /// The message to print to stderr.
    pub message: String,
    /// The exit code to exit with.
    pub exit_code: i32,
}

impl ChatServeError {
    /// A serve failure with `message` and `exit_code`.
    pub fn new(message: impl Into<String>, exit_code: i32) -> Self {
        Self {
            message: message.into(),
            exit_code,
        }
    }
}

impl std::fmt::Display for ChatServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ChatServeError {}

/// Parse `amparo chat` flags. Never panics and never exits — problems come
/// back as [`ParseChatResult::Error`].
pub fn parse_chat_flags(args: impl Iterator<Item = String>) -> ParseChatResult {
    let mut flags = ChatFlags::default();
    let mut positional: Vec<String> = Vec::new();

    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--policy-url" => match args.next() {
                Some(url) => flags.policy_url = Some(url),
                None => return ParseChatResult::Error("--policy-url requires a URL".into()),
            },
            "--allow-all" => flags.allow_all = true,
            "--growth" => flags.growth = true,
            "--no-growth" => flags.growth = false,
            "--chat-config" => match args.next() {
                Some(path) => flags.chat_config = Some(PathBuf::from(path)),
                None => return ParseChatResult::Error("--chat-config requires a path".into()),
            },
            "--auto-approve" => flags.auto_approve = true,
            "--trust-ceiling" => match args.next() {
                Some(tier) => match tier.as_str() {
                    "observational" => flags.trust_ceiling = ToolTrustTier::Observational,
                    "local_mutating" => flags.trust_ceiling = ToolTrustTier::LocalMutating,
                    "external_effector" => flags.trust_ceiling = ToolTrustTier::ExternalEffector,
                    "system_control" => flags.trust_ceiling = ToolTrustTier::SystemControl,
                    other => return ParseChatResult::Error(format!("unknown trust tier {other}")),
                },
                None => return ParseChatResult::Error("--trust-ceiling requires a tier".into()),
            },
            "--help" | "-h" => return ParseChatResult::Help,
            other if other.starts_with('-') => {
                return ParseChatResult::Error(format!(
                    "unknown flag {other}; see `amparo chat --help`"
                ))
            }
            other => positional.push(other.to_string()),
        }
    }

    if flags.policy_url.is_some() && flags.allow_all {
        return ParseChatResult::Error(
            "--policy-url and --allow-all are mutually exclusive".into(),
        );
    }
    match positional.len() {
        0 => ParseChatResult::Error(
            "amparo chat requires a platform: telegram | discord | slack".into(),
        ),
        1 => match Platform::parse(&positional[0]) {
            Some(platform) => {
                flags.platform = platform;
                ParseChatResult::Serve(flags)
            }
            None => ParseChatResult::Error(format!(
                "unknown platform '{}' — expected telegram, discord or slack",
                positional[0]
            )),
        },
        _ => ParseChatResult::Error("amparo chat takes exactly one platform".into()),
    }
}

/// The shared driver assembly every adapter's serve loop is built from.
///
/// Wiring order (identical to `amparo run`'s `execute`): the inference
/// provider fails closed from `AMPARO_INFERENCE_*`, the policy match wires
/// a [`PolicySource::Wire`] engine (`--policy-url` plus `AMPARO_POLICY_KEY`),
/// an explicit [`AllowAllPolicyEngine`], or a [`DenyAllPolicyEngine`] with
/// the same reason string as `amparo run`, then the shared
/// [`default_registry_with_memory`] (the registry itself reads `AMPARO_WORKSPACE` as
/// its root; there is no `--workspace` flag in chat flags). Tenancy: a
/// `--chat-config <path>` flag (winning over `AMPARO_CHAT_CONFIG`) loads
/// the TOML tenant directory — [`Tenants::Directory`]; a load failure is a
/// configuration problem (exit 2), and an empty directory is reported at
/// startup. Without either, the allowlist comes from `AMPARO_CHAT_ALLOWLIST`
/// — comma-separated ids, trimmed, empties dropped; absent or empty means
/// every message is refused, which is reported at startup. `transport` is
/// the platform's outbound handle; the approval router is platform-neutral
/// and built here. With `--growth`, the notebook's hot layer (M6e) is
/// opened next to the cold archive and attached via
/// [`ChatDriver::with_hot_layer`].
pub async fn build_driver(
    flags: &ChatFlags,
    transport: Arc<dyn ChatTransport>,
) -> Result<Arc<ChatDriver>, ChatServeError> {
    let config = InferenceConfig::from_env().map_err(|e| {
        ChatServeError::new(
            format!(
                "{e}\nset AMPARO_INFERENCE_URL and AMPARO_INFERENCE_MODEL — see the README \
                 Quickstart for the full environment surface"
            ),
            1,
        )
    })?;
    let provider = config
        .build()
        .map_err(|e| ChatServeError::new(e.to_string(), 1))?;

    // The memory backend (M11 W1): resolved once per chat process — the
    // Engram adapter when configured and reachable, the built-in store
    // otherwise. The allowlist arm shares this registry; the directory
    // arm's per-task registries get the same store from the driver.
    let memory = resolve_memory_backend().await;
    let mut registry = default_registry_with_memory(Arc::clone(&memory));
    // M7b: the sandbox tool is host-registered, like use_skill. This
    // registry is also the driver's legacy shared registry, so the
    // allowlist tenant arm gets eval_wasm from here.
    registry.register(Arc::new(EvalWasmTool::new()));
    // M10 W2: send_notification rides the platform transport. The
    // shared registry serves every allowlist tenant, so the adapter
    // routes by the destination argument (the approval copy names it —
    // the send itself always asks a human first).
    registry.register(Arc::new(SendNotificationTool::new(Arc::new(
        ChatNotificationTransport::new(Arc::clone(&transport), flags.platform.name()),
    ))));

    let policy_source = match (&flags.policy_url, flags.allow_all) {
        (Some(url), false) => {
            let api_key = std::env::var("AMPARO_POLICY_KEY").ok();
            PolicySource::Wire {
                base_url: url.clone(),
                api_key,
            }
        }
        (None, true) => PolicySource::Shared(Arc::new(AllowAllPolicyEngine)),
        (None, false) => PolicySource::Shared(Arc::new(DenyAllPolicyEngine::new(
            "no policy configured (--policy-url or --allow-all)",
        ))),
        (Some(_), true) => unreachable!("rejected by parse_chat_flags"),
    };

    let tenants = match flags
        .chat_config
        .clone()
        .or_else(|| std::env::var("AMPARO_CHAT_CONFIG").ok().map(PathBuf::from))
    {
        Some(path) => {
            let config =
                ChatConfig::load(&path).map_err(|e| ChatServeError::new(e.to_string(), 2))?;
            if config.is_empty() {
                eprintln!(
                    "warning: no users in chat config {} — every message will be refused",
                    path.display()
                );
            }
            if std::env::var("AMPARO_CHAT_ALLOWLIST").is_ok() {
                eprintln!("warning: AMPARO_CHAT_ALLOWLIST is ignored while a chat config is set");
            }
            Tenants::Directory(Arc::new(config))
        }
        None => {
            let allowlist = allowlist_from_env();
            if allowlist.is_empty() {
                eprintln!("no users in AMPARO_CHAT_ALLOWLIST — every message will be refused");
            }
            Tenants::LegacyAllowlist(allowlist)
        }
    };

    let router = Arc::new(ApprovalRouter::new());
    let workspace_root = std::env::var("AMPARO_WORKSPACE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    // The growth notebook's directory is resolved before the root moves
    // into the driver; without --growth nothing is opened and nothing is
    // recorded.
    let nb_dir = flags.growth.then(|| notebook_dir(&workspace_root));

    let mut driver = ChatDriver::new(
        tenants,
        provider,
        policy_source,
        registry,
        workspace_root,
        transport,
        router,
        flags.auto_approve,
    )
    .with_trust_ceiling(flags.trust_ceiling)
    .with_memory(memory);
    if let Some(nb_dir) = nb_dir {
        let path = nb_dir.join("records.jsonl");
        let store = JsonlStore::open(&path)
            .map_err(|e| ChatServeError::new(format!("cannot open the growth notebook: {e}"), 2))?;
        // The hot layer (M6e): the informative subset the case library
        // reads — the tail promoted and the layer folded by the driver at
        // every task start. Read-only by convention: the sink owns the
        // cold store, and only the rollup writes hot.
        let hot_store = JsonlStore::open(nb_dir.join(HOT_FILE)).map_err(|e| {
            ChatServeError::new(format!("cannot open the notebook hot layer: {e}"), 2)
        })?;
        eprintln!(
            "[growth] recording PII-stripped run records to {}",
            path.display()
        );
        eprintln!("[growth] retrieval: prior per-user cases (hot layer) inform self-verification");
        eprintln!("[growth] skills: per-tenant adopted skills are available to the loop");
        driver = driver
            .with_growth(Arc::new(store))
            .with_hot_layer(Arc::new(hot_store), nb_dir);
    }
    let driver = Arc::new(driver);
    // The schedule ticker (M8 W5): scan the queue every 30 s from
    // startup, whichever platform adapter is serving. Promises made by
    // directory-mode tenants with `schedule = true` fire here.
    driver.start_scheduler();
    Ok(driver)
}

/// Serve `flags.platform` until Ctrl-C or a fatal failure.
pub async fn serve(flags: &ChatFlags) -> Result<(), ChatServeError> {
    match flags.platform {
        Platform::Telegram => crate::telegram::serve(flags).await,
        Platform::Discord => crate::discord::serve(flags)
            .await
            .map_err(|e| ChatServeError::new(e.to_string(), 1)),
        Platform::Slack => crate::slack::serve(flags)
            .await
            .map_err(|e| ChatServeError::new(e.to_string(), 1)),
    }
}

/// Entry point for `amparo chat`: parse, then serve with exit codes
/// (0 = help, 2 = flag/token problem, 1 = serve failure).
pub async fn dispatch(args: impl Iterator<Item = String>) {
    match parse_chat_flags(args) {
        ParseChatResult::Help => println!("{CHAT_USAGE}"),
        ParseChatResult::Error(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
        ParseChatResult::Serve(flags) => {
            if let Err(err) = serve(&flags).await {
                eprintln!("amparo chat: {}", err.message);
                std::process::exit(err.exit_code);
            }
        }
    }
}

/// The user allowlist from `AMPARO_CHAT_ALLOWLIST`: comma-split, trimmed,
/// empties dropped. Absent or empty yields an empty set — deny everyone.
fn allowlist_from_env() -> HashSet<String> {
    let Ok(raw) = std::env::var("AMPARO_CHAT_ALLOWLIST") else {
        return HashSet::new();
    };
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> ParseChatResult {
        parse_chat_flags(args.iter().map(|s| s.to_string()))
    }

    fn flags(result: ParseChatResult) -> ChatFlags {
        match result {
            ParseChatResult::Serve(f) => f,
            ParseChatResult::Help => panic!("expected Serve, got Help"),
            ParseChatResult::Error(m) => panic!("expected Serve, got error: {m}"),
        }
    }

    fn error(result: ParseChatResult) -> String {
        match result {
            ParseChatResult::Error(m) => m,
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn platform_is_the_positional_argument() {
        assert_eq!(flags(parse(&["telegram"])).platform, Platform::Telegram);
        assert_eq!(flags(parse(&["discord"])).platform, Platform::Discord);
        assert_eq!(flags(parse(&["slack"])).platform, Platform::Slack);
    }

    #[test]
    fn parses_every_flag_with_defaults_for_the_rest() {
        let f = flags(parse(&[
            "telegram",
            "--policy-url",
            "http://policy.test",
            "--auto-approve",
            "--trust-ceiling",
            "observational",
            "--growth",
        ]));
        assert_eq!(f.policy_url.as_deref(), Some("http://policy.test"));
        assert!(f.auto_approve);
        assert!(!f.allow_all);
        assert_eq!(f.trust_ceiling, ToolTrustTier::Observational);
        assert!(f.growth);
        assert_eq!(f.platform, Platform::Telegram);
    }

    #[test]
    fn help_is_recognized() {
        assert!(matches!(parse(&["--help"]), ParseChatResult::Help));
        assert!(matches!(parse(&["-h"]), ParseChatResult::Help));
    }

    #[test]
    fn rejects_unknown_flags_missing_values_and_bad_tiers() {
        assert_eq!(
            error(parse(&["telegram", "--nonsense"])),
            "unknown flag --nonsense; see `amparo chat --help`"
        );
        assert_eq!(
            error(parse(&["telegram", "--policy-url"])),
            "--policy-url requires a URL"
        );
        assert_eq!(
            error(parse(&["--trust-ceiling"])),
            "--trust-ceiling requires a tier"
        );
        assert_eq!(
            error(parse(&["telegram", "--trust-ceiling", "nonsense"])),
            "unknown trust tier nonsense"
        );
    }

    #[test]
    fn chat_config_flag_parses() {
        let f = flags(parse(&["telegram", "--chat-config", "/tmp/tenants.toml"]));
        assert_eq!(f.chat_config, Some(PathBuf::from("/tmp/tenants.toml")));
        let f = flags(parse(&["telegram"]));
        assert_eq!(f.chat_config, None, "absent by default");
    }

    #[test]
    fn chat_config_flag_requires_a_value() {
        let message = error(parse(&["telegram", "--chat-config"]));
        assert!(message.contains("requires a path"), "{message}");
    }

    #[test]
    fn usage_mentions_chat_config() {
        assert!(
            CHAT_USAGE.contains("--chat-config"),
            "usage documents the flag"
        );
        assert!(
            CHAT_USAGE.contains("AMPARO_CHAT_CONFIG"),
            "usage documents the env var"
        );
    }

    #[test]
    fn usage_mentions_growth() {
        assert!(CHAT_USAGE.contains("--growth"), "usage documents the flag");
        assert!(
            CHAT_USAGE.contains("--no-growth"),
            "usage documents the off flag"
        );
        assert!(
            CHAT_USAGE.contains("records.jsonl"),
            "usage documents the record path"
        );
    }

    #[test]
    fn growth_flag_last_wins_and_defaults_off() {
        assert!(!flags(parse(&["telegram"])).growth);
        assert!(flags(parse(&["telegram", "--growth"])).growth);
        assert!(!flags(parse(&["telegram", "--growth", "--no-growth"])).growth);
        assert!(flags(parse(&["telegram", "--no-growth", "--growth"])).growth);
    }

    #[test]
    fn rejects_conflicting_modes_and_bad_platforms() {
        assert_eq!(
            error(parse(&[
                "telegram",
                "--policy-url",
                "http://p.test",
                "--allow-all"
            ])),
            "--policy-url and --allow-all are mutually exclusive"
        );
        assert_eq!(
            error(parse(&["--allow-all"])),
            "amparo chat requires a platform: telegram | discord | slack"
        );
        assert_eq!(
            error(parse(&["matrix", "--allow-all"])),
            "unknown platform 'matrix' — expected telegram, discord or slack"
        );
        assert_eq!(
            error(parse(&["telegram", "discord"])),
            "amparo chat takes exactly one platform"
        );
    }

    #[test]
    fn default_flags_are_deny_by_default() {
        let f = flags(parse(&["telegram"]));
        assert!(f.policy_url.is_none());
        assert!(!f.allow_all);
        assert!(!f.auto_approve);
        assert!(!f.growth);
        assert_eq!(f.trust_ceiling, ToolTrustTier::SystemControl);
    }

    #[test]
    fn allowlist_splits_trims_and_drops_empties() {
        std::env::set_var("AMPARO_CHAT_ALLOWLIST", " 111 ,, 222 , , ");
        assert_eq!(
            allowlist_from_env(),
            HashSet::from(["111".to_string(), "222".to_string()])
        );
        std::env::remove_var("AMPARO_CHAT_ALLOWLIST");
        assert!(
            allowlist_from_env().is_empty(),
            "absent allowlist denies everyone"
        );
    }
}
