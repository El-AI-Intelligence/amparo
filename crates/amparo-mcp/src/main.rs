//! `amparo-mcp-serve` — expose the default Amparo tool registry over MCP.
//!
//! Deny-by-default, explicitly: with no flags, every `tools/call` is
//! refused by the deny-all engine. `--policy-url` wires a remote engine
//! (Guardrail is a commercial implementation of the same wire protocol);
//! `--allow-all` is an explicit opt-in for local experiments. Human
//! approval stays auto-deny unless `--auto-approve` is passed — remote MCP
//! clients have no human at the terminal by default.

use amparo_agent::{AutoApprove, AutoDeny};
use amparo_mcp::McpServer;
use amparo_policy::{DenyAllPolicyEngine, PolicyDecision, PolicyEngine, wire::WirePolicyEngine};
use amparo_tools::{ToolRegistry, ToolTrustTier, default_registry};
use async_trait::async_trait;
use std::sync::Arc;

/// Explicit opt-in engine for `--allow-all` — never the default.
struct AllowAll;

#[async_trait]
impl PolicyEngine for AllowAll {
    async fn judge_tool(
        &self,
        _tool: &str,
        _target: &str,
        _params: &[(&str, &str)],
    ) -> PolicyDecision {
        PolicyDecision::allow()
    }
}

#[tokio::main]
async fn main() {
    let mut policy_url: Option<String> = None;
    let mut allow_all = false;
    let mut auto_approve = false;
    let mut ceiling = ToolTrustTier::SystemControl;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--policy-url" => {
                policy_url = Some(args.next().expect("--policy-url requires a URL"));
            }
            "--allow-all" => allow_all = true,
            "--auto-approve" => auto_approve = true,
            "--trust-ceiling" => {
                let tier = args.next().expect("--trust-ceiling requires a tier");
                ceiling = match tier.as_str() {
                    "observational" => ToolTrustTier::Observational,
                    "local_mutating" => ToolTrustTier::LocalMutating,
                    "external_effector" => ToolTrustTier::ExternalEffector,
                    "system_control" => ToolTrustTier::SystemControl,
                    other => {
                        eprintln!("unknown trust tier {other}");
                        std::process::exit(2);
                    }
                };
            }
            "--help" | "-h" => {
                println!(
                    "amparo-mcp-serve — Amparo tools over MCP (stdio, newline-delimited JSON-RPC 2.0)\n\
                     \n\
                     USAGE:\n  amparo-mcp-serve [FLAGS]\n\
                     \n\
                     FLAGS:\n\
                     \x20 --policy-url URL    wire a remote policy engine (deny-all default)\n\
                     \x20 --allow-all         run without policy checks (explicit opt-in)\n\
                     \x20 --auto-approve      approve escalated/external-effector calls\n\
                     \x20                      without a human (default: auto-deny)\n\
                     \x20 --trust-ceiling T   observational | local_mutating |\n\
                     \x20                      external_effector | system_control (default)\n"
                );
                return;
            }
            other => {
                eprintln!("unknown flag {other}; see --help");
                std::process::exit(2);
            }
        }
    }

    let policy: Arc<dyn PolicyEngine> = match (policy_url, allow_all) {
        (Some(_), true) => {
            eprintln!("--policy-url and --allow-all are mutually exclusive");
            std::process::exit(2);
        }
        (Some(url), false) => {
            let api_key = std::env::var("AMPARO_POLICY_KEY").ok();
            Arc::new(WirePolicyEngine::new(url, api_key))
        }
        (None, true) => Arc::new(AllowAll),
        (None, false) => Arc::new(DenyAllPolicyEngine::new(
            "no policy configured (--policy-url or --allow-all)",
        )),
    };

    let approval = if auto_approve {
        Arc::new(AutoApprove) as Arc<dyn amparo_agent::ApprovalGate>
    } else {
        Arc::new(AutoDeny)
    };

    let registry: ToolRegistry = default_registry();
    let server = McpServer::new(registry, policy)
        .with_approval(approval)
        .with_trust_ceiling(ceiling);
    if let Err(e) = server.serve_stdio().await {
        eprintln!("amparo-mcp-serve: {e}");
        std::process::exit(1);
    }
}
