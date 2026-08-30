//! Preflight blast-radius classification (M7).
//!
//! Before the human-approval gate asks its question, [`classify`] labels
//! the call with a [`BlastRadius`] — what executing it could touch: a
//! read-only look, the workspace, a sub-agent spawn, the network, the
//! wider system, or a destructive pattern. The label rides on
//! [`crate::approval::ApprovalRequest::blast_radius`] and is rendered in
//! the approval copy, so the human approves a *concrete consequence*, not
//! an abstraction.
//!
//! **Display-only by design (I1).** Classification runs *after* the gate
//! chain has decided the call may execute, and nothing in this module
//! feeds back into the gate, the policy engine, or the prompts — a wrong
//! label can only misdescribe the approval text, never allow or block
//! anything. The seed is the tool's registered trust tier; argument
//! inspection can only *raise* the label (a call that matches a blocked
//! destructive pattern is labeled `destructive` even though the gate
//! would have blocked it independently).

use amparo_tools::{PathPolicy, ToolCall, ToolRegistry, ToolTrustTier};
use serde::{Deserialize, Serialize};

/// How far a tool call could reach, worst case. Ordered by severity:
/// [`BlastRadius::ReadOnly`] < [`BlastRadius::WorkspaceLocal`] <
/// [`BlastRadius::SubAgent`] < [`BlastRadius::Network`] <
/// [`BlastRadius::SystemWide`] < [`BlastRadius::Destructive`] (the
/// discriminants are the rank).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum BlastRadius {
    /// Observes only — reads and searches, no mutation anywhere.
    ReadOnly = 0,
    /// Mutates only inside the workspace (or the shared scratch dirs).
    WorkspaceLocal = 1,
    /// Spawns a sub-agent that acts under the same gate chain — its own
    /// calls carry their own labels, so the spawn itself ranks below a
    /// direct network reach but above a plain local mutation.
    SubAgent = 2,
    /// Reaches the network (or other external effects short of the
    /// machine's own configuration).
    Network = 3,
    /// Touches the wider system — files outside the workspace, or
    /// system-control tooling.
    SystemWide = 4,
    /// Matches a blocked destructive pattern (the gate blocks these
    /// independently; the label says how bad the ask was).
    Destructive = 5,
}

impl BlastRadius {
    /// The severity rank: 0 ([`BlastRadius::ReadOnly`]) through 5
    /// ([`BlastRadius::Destructive`]).
    pub fn severity(self) -> u8 {
        self as u8
    }

    /// The one-line plain-language note rendered in the approval copy:
    /// what approving this call means in concrete terms.
    pub fn note(self) -> &'static str {
        match self {
            Self::ReadOnly => "observes only; nothing is modified",
            Self::WorkspaceLocal => "changes stay inside the workspace",
            Self::SubAgent => "spawns a sub-agent that acts under the same gate chain",
            Self::Network => "reaches the network",
            Self::SystemWide => "touches files outside the workspace",
            Self::Destructive => "matches a blocked destructive pattern",
        }
    }
}

impl std::fmt::Display for BlastRadius {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadOnly => write!(f, "read_only"),
            Self::WorkspaceLocal => write!(f, "workspace_local"),
            Self::SubAgent => write!(f, "sub_agent"),
            Self::Network => write!(f, "network"),
            Self::SystemWide => write!(f, "system_wide"),
            Self::Destructive => write!(f, "destructive"),
        }
    }
}

/// The file tools whose `path` argument gets the workspace-boundary
/// inspection: an absolute path outside the workspace, `/tmp` and
/// `/dev/shm` labels the call [`BlastRadius::SystemWide`].
const FILE_TOOLS: &[&str] = &["read_file", "write_file", "edit_file", "patch_file", "list_dir"];

/// Command tokens that transfer data over the network. Matched as whole
/// shell words (case-insensitive), so `sync` does not trip `nc`.
const NETWORK_COMMANDS: &[&str] = &["curl", "wget", "nc", "scp", "rsync", "ssh"];

/// Classify one call's blast radius. The trust tier seeds the class;
/// argument inspection can only raise it (first raise wins):
///
/// 1. `run_command` whose command matches
///    [`PathPolicy::check_command_blocked`] → [`BlastRadius::Destructive`]
/// 2. `run_command` naming a network-transfer utility → at least
///    [`BlastRadius::Network`]
/// 3. a file tool given an absolute path outside the workspace, `/tmp`
///    and `/dev/shm` → at least [`BlastRadius::SystemWide`]
///
/// An unknown tool (never seen in the loop — the gate blocks those
/// first) is labeled [`BlastRadius::SystemWide`], the honest
/// worst-case claim short of a destructive match.
pub fn classify(registry: &ToolRegistry, policy: &PathPolicy, call: &ToolCall) -> BlastRadius {
    // Static override (M7b): eval_wasm computes inside a sealed sandbox —
    // no imports, no WASI — so it observes and modifies nothing outside
    // its own memory. Its tier stays ExternalEffector (untrusted code
    // always asks a human); the radius says what executing it can touch:
    // nothing. Display-only, like everything here.
    if call.name == "eval_wasm" {
        return BlastRadius::ReadOnly;
    }

    // Static override (M8): a spawn is not itself a network or system
    // effect — the child's own calls carry their own labels. The tier
    // stays ExternalEffector (creating an acting entity always asks a
    // human); the radius says what the spawn itself can touch.
    if call.name == "spawn_agent" {
        return BlastRadius::SubAgent;
    }

    let mut radius = match registry.get_tier(&call.name) {
        Some(ToolTrustTier::Observational) => BlastRadius::ReadOnly,
        Some(ToolTrustTier::LocalMutating) => BlastRadius::WorkspaceLocal,
        Some(ToolTrustTier::ExternalEffector) => BlastRadius::Network,
        Some(ToolTrustTier::SystemControl) => BlastRadius::SystemWide,
        None => return BlastRadius::SystemWide,
    };

    if call.name == "run_command" {
        if let Some(command) = call.arg_str("command") {
            if policy.check_command_blocked(command).is_some() {
                return BlastRadius::Destructive;
            }
            if transfers_over_network(command) {
                radius = radius.max(BlastRadius::Network);
            }
        }
    }

    if FILE_TOOLS.contains(&call.name.as_str()) {
        if let Some(path) = call.arg_str("path") {
            if outside_workspace(path, &policy.workspace_root) {
                radius = radius.max(BlastRadius::SystemWide);
            }
        }
    }

    radius
}

/// Does `command` name a network-transfer utility as a whole shell word?
fn transfers_over_network(command: &str) -> bool {
    command
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|token| NETWORK_COMMANDS.iter().any(|known| token.eq_ignore_ascii_case(known)))
}

/// Is `path` an absolute path outside the workspace and the shared
/// scratch dirs? Intent-level: no canonicalization — the label
/// describes what the model *asked for*, not what the remapping rules
/// would reduce it to.
fn outside_workspace(path: &str, workspace_root: &std::path::Path) -> bool {
    let path = std::path::Path::new(path);
    if !path.is_absolute() {
        return false;
    }
    if path.starts_with(workspace_root) {
        return false;
    }
    if path.starts_with("/tmp") || path.starts_with("/dev/shm") {
        return false;
    }
    true
}

// ───────────────────────────────────────────────────────────── Tests ─────────

#[cfg(test)]
mod tests {
    use super::*;
    use amparo_tools::registry::{ToolSchema, ToolExecutor, ToolParam};
    use async_trait::async_trait;
    use std::path::PathBuf;
    use std::sync::Arc;

    /// A minimal executor so the tests can register tools at any tier.
    struct StubTool {
        name: &'static str,
        tier: ToolTrustTier,
    }

    #[async_trait]
    impl ToolExecutor for StubTool {
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: self.name.to_string(),
                description: "stub".to_string(),
                parameters: Vec::<ToolParam>::new(),
                trust_tier: self.tier,
            }
        }

        async fn execute(&self, _call: &ToolCall) -> amparo_tools::ToolResult {
            amparo_tools::ToolResult {
                tool_call_id: "call".to_string(),
                tool_name: self.name.to_string(),
                success: true,
                output: serde_json::json!({}),
                display_summary: "ok".to_string(),
                duration_ms: 0,
            }
        }
    }

    fn registry(tools: &[(&'static str, ToolTrustTier)]) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        for (name, tier) in tools {
            registry.register(Arc::new(StubTool { name: *name, tier: *tier }));
        }
        registry
    }

    fn policy() -> PathPolicy {
        PathPolicy::from_root(PathBuf::from("/tmp/amparo-ws"))
    }

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall { id: "call_1".to_string(), name: name.to_string(), arguments }
    }

    #[test]
    fn severity_ranks_are_monotonic() {
        assert_eq!(BlastRadius::ReadOnly.severity(), 0);
        assert_eq!(BlastRadius::WorkspaceLocal.severity(), 1);
        assert_eq!(BlastRadius::SubAgent.severity(), 2);
        assert_eq!(BlastRadius::Network.severity(), 3);
        assert_eq!(BlastRadius::SystemWide.severity(), 4);
        assert_eq!(BlastRadius::Destructive.severity(), 5);
        assert!(BlastRadius::ReadOnly < BlastRadius::Destructive);
        assert!(BlastRadius::SubAgent < BlastRadius::Network);
    }

    #[test]
    fn display_names_are_snake_case() {
        assert_eq!(BlastRadius::ReadOnly.to_string(), "read_only");
        assert_eq!(BlastRadius::WorkspaceLocal.to_string(), "workspace_local");
        assert_eq!(BlastRadius::SubAgent.to_string(), "sub_agent");
        assert_eq!(BlastRadius::Network.to_string(), "network");
        assert_eq!(BlastRadius::SystemWide.to_string(), "system_wide");
        assert_eq!(BlastRadius::Destructive.to_string(), "destructive");
    }

    #[test]
    fn notes_explain_the_consequence() {
        assert_eq!(BlastRadius::ReadOnly.note(), "observes only; nothing is modified");
        assert_eq!(BlastRadius::WorkspaceLocal.note(), "changes stay inside the workspace");
        assert_eq!(
            BlastRadius::SubAgent.note(),
            "spawns a sub-agent that acts under the same gate chain"
        );
        assert_eq!(BlastRadius::Network.note(), "reaches the network");
        assert_eq!(BlastRadius::SystemWide.note(), "touches files outside the workspace");
        assert_eq!(BlastRadius::Destructive.note(), "matches a blocked destructive pattern");
    }

    #[test]
    fn trust_tier_seeds_the_class() {
        let registry = registry(&[
            ("observe", ToolTrustTier::Observational),
            ("mutate", ToolTrustTier::LocalMutating),
            ("effect", ToolTrustTier::ExternalEffector),
            ("control", ToolTrustTier::SystemControl),
        ]);
        let policy = policy();
        let bare = serde_json::json!({});
        assert_eq!(classify(&registry, &policy, &call("observe", bare.clone())), BlastRadius::ReadOnly);
        assert_eq!(classify(&registry, &policy, &call("mutate", bare.clone())), BlastRadius::WorkspaceLocal);
        assert_eq!(classify(&registry, &policy, &call("effect", bare.clone())), BlastRadius::Network);
        assert_eq!(classify(&registry, &policy, &call("control", bare)), BlastRadius::SystemWide);
    }

    #[test]
    fn unknown_tool_is_labeled_system_wide() {
        let registry = registry(&[]);
        assert_eq!(
            classify(&registry, &policy(), &call("no_such_tool", serde_json::json!({}))),
            BlastRadius::SystemWide
        );
    }

    #[test]
    fn destructive_pattern_raises_to_destructive() {
        let registry = registry(&[("run_command", ToolTrustTier::ExternalEffector)]);
        let policy = policy();
        let destructive = call("run_command", serde_json::json!({"command": "sudo rm -rf /"}));
        assert_eq!(classify(&registry, &policy, &destructive), BlastRadius::Destructive);
        // Benign commands keep the tier seed.
        let benign = call("run_command", serde_json::json!({"command": "git status"}));
        assert_eq!(classify(&registry, &policy, &benign), BlastRadius::Network);
    }

    #[test]
    fn network_transfer_utility_raises_to_network() {
        // Registered below its usual tier to prove the argument check
        // raises the seed on its own. `ssh` is not on the default
        // blocklist, so the network refinement is what fires.
        let registry = registry(&[("run_command", ToolTrustTier::Observational)]);
        let policy = policy();
        let cmd = call("run_command", serde_json::json!({"command": "ssh deploy@box"}));
        assert_eq!(classify(&registry, &policy, &cmd), BlastRadius::Network);
    }

    #[test]
    fn blocked_network_transfer_labels_destructive_first() {
        // `curl` is blocked by the default policy, so the destructive
        // match wins over the network refinement — first raise wins.
        let registry = registry(&[("run_command", ToolTrustTier::Observational)]);
        let policy = policy();
        let cmd = call("run_command", serde_json::json!({"command": "curl -s http://x"}));
        assert_eq!(classify(&registry, &policy, &cmd), BlastRadius::Destructive);
    }

    #[test]
    fn word_boundaries_keep_plain_commands_local() {
        // "nc" inside "sync" is not a network transfer.
        let registry = registry(&[("run_command", ToolTrustTier::Observational)]);
        let policy = policy();
        let call = call("run_command", serde_json::json!({"command": "sync"}));
        assert_eq!(classify(&registry, &policy, &call), BlastRadius::ReadOnly);
    }

    #[test]
    fn file_paths_outside_the_workspace_raise_to_system_wide() {
        let registry = registry(&[("write_file", ToolTrustTier::LocalMutating)]);
        let policy = policy();
        let outside = call("write_file", serde_json::json!({"path": "/etc/passwd"}));
        assert_eq!(classify(&registry, &policy, &outside), BlastRadius::SystemWide);
        // The shared scratch dirs and the workspace itself stay local…
        let scratch = call("write_file", serde_json::json!({"path": "/tmp/out.txt"}));
        assert_eq!(classify(&registry, &policy, &scratch), BlastRadius::WorkspaceLocal);
        let inside = call("write_file", serde_json::json!({"path": "/tmp/amparo-ws/out.txt"}));
        assert_eq!(classify(&registry, &policy, &inside), BlastRadius::WorkspaceLocal);
        // …and relative paths never leave the workspace.
        let relative = call("write_file", serde_json::json!({"path": "notes.txt"}));
        assert_eq!(classify(&registry, &policy, &relative), BlastRadius::WorkspaceLocal);
    }

    #[test]
    fn spawn_agent_is_labeled_sub_agent_despite_the_effector_tier() {
        // Spawning always asks a human (ExternalEffector), but the radius
        // is its own class: the child's calls carry their own labels.
        let registry = registry(&[("spawn_agent", ToolTrustTier::ExternalEffector)]);
        let policy = policy();
        let call = call("spawn_agent", serde_json::json!({"task": "research it"}));
        assert_eq!(classify(&registry, &policy, &call), BlastRadius::SubAgent);
        assert_eq!(
            registry.get_tier("spawn_agent"),
            Some(ToolTrustTier::ExternalEffector)
        );
    }

    #[test]
    fn eval_wasm_is_read_only_despite_the_effector_tier() {
        // The sandbox tool is registered at ExternalEffector — executing
        // untrusted code always asks a human — but its blast radius is
        // read-only: a sealed module can touch nothing outside its own
        // memory. The override is display-only; the tier is unchanged.
        let registry = registry(&[("eval_wasm", ToolTrustTier::ExternalEffector)]);
        let policy = policy();
        let call = call("eval_wasm", serde_json::json!({"wasm_base64": "AGFzbQE="}));
        assert_eq!(classify(&registry, &policy, &call), BlastRadius::ReadOnly);
        assert_eq!(
            registry.get_tier("eval_wasm"),
            Some(ToolTrustTier::ExternalEffector)
        );
    }

    #[test]
    fn non_file_tools_skip_the_path_inspection() {
        // fetch_url takes no "path"; its Network seed stands regardless.
        let registry = registry(&[("fetch_url", ToolTrustTier::ExternalEffector)]);
        let policy = policy();
        let call = call("fetch_url", serde_json::json!({"url": "http://example.com"}));
        assert_eq!(classify(&registry, &policy, &call), BlastRadius::Network);
    }
}
