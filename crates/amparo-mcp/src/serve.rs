//! The `mcp-serve` entry point as a library module.
//!
//! Shared verbatim by the standalone `amparo-mcp-serve` binary and the
//! `amparo mcp-serve` subcommand, so help text, error strings and exit codes
//! stay identical across both entry points. Posture: deny-by-default — with
//! no flags every `tools/call` is refused by the deny-all engine,
//! `--policy-url` wires a remote engine (Guardrail is a commercial
//! implementation of the same wire protocol), and `--allow-all` is an
//! explicit opt-in for local experiments. Human approval stays auto-deny
//! unless `--auto-approve` is passed — remote MCP clients have no human at
//! the terminal by default.

use amparo_agent::{
    format_event, mint_agent_id, valid_approval_endpoint, Agent, AgentConfig, AgentEvent,
    ApprovalGate, AutoApprove, AutoDeny, EventSink, JsonCheckpointStore, SpawnAgentTool,
    WebApprovalGate,
};
use amparo_inference::InferenceConfig;
use amparo_policy::{
    wire::WirePolicyEngine, AllowAllPolicyEngine, AuditNoticeEngine, DenyAllPolicyEngine,
    PolicyEngine,
};
use amparo_sandbox::EvalWasmTool;
use amparo_tools::{
    default_registry_with_memory, resolve_memory_backend, PathPolicy, ToolRegistry, ToolTrustTier,
};
use std::sync::{Arc, Mutex};

use crate::McpServer;

/// Renders child-agent events as `[tag]` stderr lines (M10 W5): the MCP
/// channel keeps stdout protocol-only, so a spawned child's event stream
/// joins the server's logs — the same stderr contract `amparo run` keeps.
struct StderrEventSink;

impl EventSink for StderrEventSink {
    fn emit(&self, event: &AgentEvent) {
        eprintln!("{}", format_event(event));
    }
}

/// Parsed command-line flags for a serve invocation.
#[derive(Debug, Clone)]
pub struct ServeFlags {
    /// Wire a remote policy engine at this URL (deny-all default).
    pub policy_url: Option<String>,
    /// Run without policy checks — explicit opt-in, never the default.
    pub allow_all: bool,
    /// Auto-approve escalated/external-effector calls (default: auto-deny).
    pub auto_approve: bool,
    /// Highest trust tier the server will execute.
    pub trust_ceiling: ToolTrustTier,
    /// Session id attached to every policy check (M9 W3): correlates
    /// engine-side audit rows with the caller behind this server.
    pub session_id: Option<String>,
    /// Web-approval endpoint (M10 W4): when set, approval requests POST
    /// to this URL and the gate polls it for the human's decision.
    /// Mutually exclusive with `--auto-approve`.
    pub approval_endpoint: Option<String>,
    /// Swarm budget (M10 W5): register `spawn_agent` with at most N
    /// sub-agents per spawn chain, all drawing from one shared counter.
    /// `0` (the default) keeps the tool off — an MCP server spawns no
    /// agents unless asked.
    pub max_sub_agents: usize,
}

impl Default for ServeFlags {
    fn default() -> Self {
        Self {
            policy_url: None,
            allow_all: false,
            auto_approve: false,
            trust_ceiling: ToolTrustTier::SystemControl,
            session_id: None,
            approval_endpoint: None,
            max_sub_agents: 0,
        }
    }
}

/// Outcome of [`parse_flags`]: serve with these flags, print [`HELP`] to
/// stdout and exit 0, or print the message to stderr and exit 2.
#[derive(Debug)]
pub enum ParseResult {
    /// Run the server with these flags.
    Serve(ServeFlags),
    /// `--help`/`-h` was passed.
    Help,
    /// A flag problem — the message carries the stderr line.
    Error(String),
}

/// Exact help text printed on `--help` (byte-identical across both entry
/// points; the trailing newline pairs with `println!`).
pub const HELP: &str =
    "amparo-mcp-serve — Amparo tools over MCP (stdio, newline-delimited JSON-RPC 2.0)\n\
     \n\
     USAGE:\n  amparo-mcp-serve [FLAGS]\n\
     \n\
     FLAGS:\n\
     \x20 --policy-url URL    wire a remote policy engine (deny-all default)\n\
     \x20 --session-id ID     tag every policy check with ID (engine-side\n\
     \x20                      audit rows)\n\
     \x20 --allow-all         run without policy checks (explicit opt-in)\n\
     \x20 --auto-approve      approve escalated/external-effector calls\n\
     \x20                      without a human (default: auto-deny)\n\
     \x20 --approval-endpoint URL  ask a web UI for approvals: POST each\n\
     \x20                      request to URL, poll URL/<call_id> for the\n\
     \x20                      decision (60s fail-closed)\n\
     \x20 --trust-ceiling T   observational | local_mutating |\n\
     \x20                      external_effector | system_control (default)\n\
     \x20 --max-sub-agents N  register spawn_agent with a shared N spawn\n\
     \x20                      budget (default 0: no sub-agents; the\n\
     \x20                      children run on AMPARO_INFERENCE_URL)\n";

/// Parse serve flags from an argument iterator (usually
/// `env::args().skip(1)`). Never panics and never exits — problems come back
/// as [`ParseResult::Error`] so both binaries keep identical exit-code
/// behavior.
pub fn parse_flags(args: impl Iterator<Item = String>) -> ParseResult {
    let mut flags = ServeFlags::default();
    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--policy-url" => match args.next() {
                Some(url) => flags.policy_url = Some(url),
                None => return ParseResult::Error("--policy-url requires a URL".to_string()),
            },
            "--session-id" => match args.next() {
                Some(id) => flags.session_id = Some(id),
                None => return ParseResult::Error("--session-id requires an id".to_string()),
            },
            "--allow-all" => flags.allow_all = true,
            "--auto-approve" => flags.auto_approve = true,
            "--approval-endpoint" => match args.next() {
                Some(url) => flags.approval_endpoint = Some(url),
                None => {
                    return ParseResult::Error("--approval-endpoint requires a URL".to_string())
                }
            },
            "--trust-ceiling" => match args.next() {
                Some(tier) => match tier.as_str() {
                    "observational" => flags.trust_ceiling = ToolTrustTier::Observational,
                    "local_mutating" => flags.trust_ceiling = ToolTrustTier::LocalMutating,
                    "external_effector" => flags.trust_ceiling = ToolTrustTier::ExternalEffector,
                    "system_control" => flags.trust_ceiling = ToolTrustTier::SystemControl,
                    other => return ParseResult::Error(format!("unknown trust tier {other}")),
                },
                None => return ParseResult::Error("--trust-ceiling requires a tier".to_string()),
            },
            "--max-sub-agents" => match args.next() {
                Some(n) => match n.parse::<usize>() {
                    Ok(max) => flags.max_sub_agents = max,
                    Err(_) => {
                        return ParseResult::Error(format!(
                            "--max-sub-agents needs a number, got '{n}'"
                        ))
                    }
                },
                None => {
                    return ParseResult::Error("--max-sub-agents requires a number".to_string())
                }
            },
            "--help" | "-h" => return ParseResult::Help,
            other => return ParseResult::Error(format!("unknown flag {other}; see --help")),
        }
    }
    if flags.approval_endpoint.is_some() && flags.auto_approve {
        return ParseResult::Error(
            "--approval-endpoint and --auto-approve are mutually exclusive".to_string(),
        );
    }
    if let Some(url) = &flags.approval_endpoint {
        if !valid_approval_endpoint(url) {
            return ParseResult::Error(format!(
                "--approval-endpoint must be an http(s) URL with a host, got '{url}'"
            ));
        }
    }
    ParseResult::Serve(flags)
}

/// A serve failure with the process exit code to use.
#[derive(Debug)]
pub struct ServeError {
    /// The full stderr line to print.
    pub message: String,
    /// 2 = flag/policy configuration problem, 1 = server failure.
    pub exit_code: i32,
}

impl ServeError {
    fn config(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: 2,
        }
    }

    fn serve(e: impl std::fmt::Display) -> Self {
        Self {
            message: format!("amparo-mcp-serve: {e}"),
            exit_code: 1,
        }
    }
}

/// Build the policy, approval gate, registry and server from [`ServeFlags`],
/// then serve over stdio until the client disconnects.
pub async fn run(flags: ServeFlags) -> Result<(), ServeError> {
    let policy: Arc<dyn PolicyEngine> = match (&flags.policy_url, flags.allow_all) {
        (Some(_), true) => {
            return Err(ServeError::config(
                "--policy-url and --allow-all are mutually exclusive",
            ));
        }
        (Some(url), false) => {
            let api_key = std::env::var("AMPARO_POLICY_KEY").ok();
            // M9 W3: every check carries the caller's session id (when
            // given), and the audit-mode notice prints once per process.
            let engine = WirePolicyEngine::new(url.clone(), api_key);
            let engine = match &flags.session_id {
                Some(id) => engine
                    .with_session_id(id.clone())
                    .with_agent_id(mint_agent_id(id)),
                None => engine,
            };
            Arc::new(AuditNoticeEngine::new(engine))
        }
        (None, true) => Arc::new(AllowAllPolicyEngine),
        (None, false) => Arc::new(DenyAllPolicyEngine::new(
            "no policy configured (--policy-url or --allow-all)",
        )),
    };

    let approval: Arc<dyn ApprovalGate> = match (flags.auto_approve, &flags.approval_endpoint) {
        (true, Some(_)) => unreachable!("rejected by parse_flags"),
        (true, None) => Arc::new(AutoApprove),
        (false, Some(url)) => Arc::new(WebApprovalGate::new(url.clone())),
        (false, None) => Arc::new(AutoDeny),
    };

    // The memory backend (M11 W1): the Engram adapter when configured
    // and reachable, the built-in store otherwise (with one `[memory]`
    // warning on the degrade path).
    let memory = resolve_memory_backend().await;
    let mut registry: ToolRegistry = default_registry_with_memory(memory);
    // M7b: eval_wasm is served over MCP too; approval defaults to
    // AutoDeny here, so it is refused until an operator allows.
    registry.register(Arc::new(EvalWasmTool::new()));
    // M10 W5: with --max-sub-agents N the server registers spawn_agent —
    // the donor agent captures the parts as they now stand (a registry
    // without itself), so every child starts spawn-free and re-registers
    // a child-flavored tool under the one shared budget. The spawn call
    // itself is a gated tools/call exactly like any other (tier
    // ExternalEffector, so the approval gate sees it).
    if let Some(tool) = spawn_tool(
        &flags,
        &registry,
        Arc::clone(&policy),
        Arc::clone(&approval),
    )? {
        registry.register(tool);
    }
    let mut server = McpServer::new(registry, policy)
        .with_approval(approval)
        .with_trust_ceiling(flags.trust_ceiling)
        // M10 W4: the env-derived path policy (the one the registry's
        // tools captured at construction) drives the full preflight
        // classification on approval requests — display-only (I1).
        .with_path_policy(Arc::new(PathPolicy::from_env()));
    if let Some(id) = &flags.session_id {
        server = server.with_session_id(id.clone());
    }
    server.serve_stdio().await.map_err(ServeError::serve)
}

/// Build the opt-in `spawn_agent` tool for this server (M10 W5) — `None`
/// when the flag is off. The tool is built off a donor agent that
/// mirrors the server's own parts (policy, approval gate, path policy,
/// trust ceiling) plus an env-derived inference provider for the
/// children's loops — the same `AMPARO_INFERENCE_*` surface `amparo run`
/// uses, fail-closed (a missing provider is a usage error, never a
/// silent default). Every child runs the full gate chain under the one
/// shared spawn budget, and checkpoints land under the workspace like
/// any run (tenant `mcp`).
fn spawn_tool(
    flags: &ServeFlags,
    registry: &ToolRegistry,
    policy: Arc<dyn PolicyEngine>,
    approval: Arc<dyn ApprovalGate>,
) -> Result<Option<Arc<SpawnAgentTool>>, ServeError> {
    if flags.max_sub_agents == 0 {
        return Ok(None);
    }
    let config = InferenceConfig::from_env().map_err(|e| {
        ServeError::config(format!(
            "{e}\nset AMPARO_INFERENCE_URL and AMPARO_INFERENCE_MODEL — see the README \
             Quickstart for the full environment surface"
        ))
    })?;
    let provider = config
        .build()
        .map_err(|e| ServeError::config(e.to_string()))?;
    // The child chain id root: the caller's session id when given (so
    // children chain as `web-1.1`), else a per-process id.
    let parent_task_id = flags
        .session_id
        .clone()
        .unwrap_or_else(|| format!("mcp-{}", std::process::id()));
    let workspace_root = PathPolicy::from_env().workspace_root;
    let donor = Agent::new(provider, registry.clone(), policy)
        .with_approval(approval)
        .with_events(Arc::new(StderrEventSink))
        .with_privacy(Arc::new(amparo_privacy::PrivacyPolicy::default()))
        .with_path_policy(Arc::new(PathPolicy::from_env()))
        .with_checkpoints(Arc::new(JsonCheckpointStore::new(&workspace_root)), "mcp")
        .with_task_id(parent_task_id.clone())
        .with_config(AgentConfig {
            trust_ceiling: flags.trust_ceiling,
            ..AgentConfig::default()
        });
    let budget = Arc::new(Mutex::new(flags.max_sub_agents));
    Ok(Some(Arc::new(SpawnAgentTool::new(
        &donor,
        parent_task_id,
        budget,
        flags.max_sub_agents,
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> ParseResult {
        parse_flags(args.iter().map(|s| s.to_string()))
    }

    fn flags(result: ParseResult) -> ServeFlags {
        match result {
            ParseResult::Serve(f) => f,
            ParseResult::Help => panic!("expected Serve, got Help"),
            ParseResult::Error(m) => panic!("expected Serve, got error: {m}"),
        }
    }

    #[test]
    fn no_flags_defaults_to_deny_all_and_system_ceiling() {
        let f = flags(parse(&[]));
        assert!(f.policy_url.is_none());
        assert!(!f.allow_all);
        assert!(!f.auto_approve);
        assert_eq!(f.trust_ceiling, ToolTrustTier::SystemControl);
    }

    #[test]
    fn parses_every_flag() {
        let f = flags(parse(&[
            "--policy-url",
            "http://policy.test",
            "--session-id",
            "web-1",
            "--auto-approve",
            "--trust-ceiling",
            "observational",
        ]));
        assert_eq!(f.policy_url.as_deref(), Some("http://policy.test"));
        assert_eq!(f.session_id.as_deref(), Some("web-1"));
        assert!(f.auto_approve);
        assert!(!f.allow_all);
        assert_eq!(f.trust_ceiling, ToolTrustTier::Observational);

        let f = flags(parse(&["--allow-all", "--trust-ceiling", "local_mutating"]));
        assert!(f.allow_all);
        assert!(f.session_id.is_none());
        assert_eq!(f.trust_ceiling, ToolTrustTier::LocalMutating);
    }

    #[test]
    fn help_is_recognized() {
        assert!(matches!(parse(&["--help"]), ParseResult::Help));
        assert!(matches!(parse(&["-h"]), ParseResult::Help));
    }

    #[test]
    fn unknown_flags_are_rejected_with_the_exact_line() {
        match parse(&["--nonsense"]) {
            ParseResult::Error(m) => assert_eq!(m, "unknown flag --nonsense; see --help"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn flags_that_take_values_reject_missing_values() {
        match parse(&["--policy-url"]) {
            ParseResult::Error(m) => assert_eq!(m, "--policy-url requires a URL"),
            other => panic!("expected Error, got {other:?}"),
        }
        match parse(&["--session-id"]) {
            ParseResult::Error(m) => assert_eq!(m, "--session-id requires an id"),
            other => panic!("expected Error, got {other:?}"),
        }
        match parse(&["--trust-ceiling"]) {
            ParseResult::Error(m) => assert_eq!(m, "--trust-ceiling requires a tier"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn unknown_tiers_are_rejected_with_the_exact_line() {
        match parse(&["--trust-ceiling", "nonsense"]) {
            ParseResult::Error(m) => assert_eq!(m, "unknown trust tier nonsense"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn help_text_names_the_binary_and_the_ceiling_flag() {
        assert!(HELP.contains("amparo-mcp-serve"));
        assert!(HELP.contains("--trust-ceiling"));
    }

    #[test]
    fn parses_the_approval_endpoint() {
        let f = flags(parse(&["--approval-endpoint", "http://web.test/approvals"]));
        assert_eq!(
            f.approval_endpoint.as_deref(),
            Some("http://web.test/approvals")
        );
        assert!(!f.auto_approve);
        let f = flags(parse(&[]));
        assert!(f.approval_endpoint.is_none());
    }

    #[test]
    fn approval_endpoint_rejects_missing_and_non_http_urls() {
        match parse(&["--approval-endpoint"]) {
            ParseResult::Error(m) => assert_eq!(m, "--approval-endpoint requires a URL"),
            other => panic!("expected Error, got {other:?}"),
        }
        match parse(&["--approval-endpoint", "nonsense"]) {
            ParseResult::Error(m) => {
                assert_eq!(
                    m,
                    "--approval-endpoint must be an http(s) URL with a host, got 'nonsense'"
                )
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn approval_endpoint_and_auto_approve_are_mutually_exclusive() {
        match parse(&["--approval-endpoint", "http://web.test", "--auto-approve"]) {
            ParseResult::Error(m) => {
                assert_eq!(
                    m,
                    "--approval-endpoint and --auto-approve are mutually exclusive"
                )
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn help_text_names_the_approval_endpoint_flag() {
        assert!(HELP.contains("--approval-endpoint"));
    }

    #[test]
    fn parses_the_max_sub_agents_flag_with_zero_default() {
        assert_eq!(flags(parse(&[])).max_sub_agents, 0);
        assert_eq!(flags(parse(&["--max-sub-agents", "3"])).max_sub_agents, 3);
        // Zero is the explicit off switch.
        assert_eq!(flags(parse(&["--max-sub-agents", "0"])).max_sub_agents, 0);
    }

    #[test]
    fn max_sub_agents_rejects_missing_and_non_numeric_values() {
        match parse(&["--max-sub-agents"]) {
            ParseResult::Error(m) => assert_eq!(m, "--max-sub-agents requires a number"),
            other => panic!("expected Error, got {other:?}"),
        }
        match parse(&["--max-sub-agents", "many"]) {
            ParseResult::Error(m) => {
                assert_eq!(m, "--max-sub-agents needs a number, got 'many'")
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn help_text_names_the_max_sub_agents_flag() {
        assert!(HELP.contains("--max-sub-agents"));
    }
}
