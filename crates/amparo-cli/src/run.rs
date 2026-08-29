//! The `amparo run` subcommand: parse flags, wire the gate chain, run the loop.
//!
//! Wiring order matters and is deliberate:
//!
//! 1. [`InferenceConfig::from_env`] fails closed — no silent localhost
//!    defaults. `--timeout` clamps to the same 1–3600s bounds as the env
//!    surface.
//! 2. `AMPARO_WORKSPACE` is set **before** [`default_registry`] — tools
//!    capture the path policy at construction.
//! 3. The policy match is identical to `amparo_mcp::serve`: wire engine,
//!    explicit allow-all, or deny-all with the same reason string.
//! 4. Approval defaults to the interactive terminal gate;
//!    `--auto-approve`/`--auto-deny` never construct it, so a piped
//!    `</dev/null` agent cannot hang on stdin.
//!
//! stdout carries the final answer only; the report goes to stderr.

use amparo_agent::{
    Agent, AgentConfig, ApprovalGate, AutoApprove, AutoDeny, EventSink, FanoutSink, TaskStatus,
};
use amparo_inference::{InferenceConfig, MAX_TIMEOUT_SECS};
use amparo_notebook::{JsonlStore, NotebookSink};
use amparo_policy::{
    AllowAllPolicyEngine, DenyAllPolicyEngine, PolicyEngine, wire::WirePolicyEngine,
};
use amparo_tools::{PathPolicy, ToolTrustTier, default_registry};
use std::sync::Arc;

use crate::approve::InteractiveApprovalGate;
use crate::events::PrintingSink;

pub const RUN_USAGE: &str = "\
amparo run — drive the agent loop end-to-end

USAGE:
  amparo run [FLAGS] \"task\"

FLAGS:
  --policy-url URL    wire a remote policy engine (deny-all default)
  --allow-all         run without policy checks (explicit opt-in)
  --auto-approve      approve escalated/external-effector calls without a human
  --auto-deny         deny escalated/external-effector calls without asking
  --growth            record PII-stripped run records (off by default)
  --no-growth         never record (overrides an earlier --growth)
  --trust-ceiling T   observational | local_mutating |
                      external_effector | system_control (default)
  --max-steps N       maximum loop iterations (default 12)
  --model M           override AMPARO_INFERENCE_MODEL for this run
  --timeout SECS      per-request timeout in seconds (clamped 1-3600)
  --workspace DIR     working directory the tools are confined to
                      (sets AMPARO_WORKSPACE)

The task is the joined positional arguments. stdout carries the final answer
only; progress, gate decisions and the report go to stderr.

Deny-by-default: without --policy-url or --allow-all every tool call is
refused, and escalated/external-effector calls ask for approval unless
--auto-approve/--auto-deny overrides. The inference endpoint comes from the
AMPARO_INFERENCE_* environment surface — see the README Quickstart.

--growth enables the lab notebook: every completed or failed task is
recorded as a PII-stripped, tenant-tagged JSON line at
<workspace>/.amparo/notebook/records.jsonl (task text, tool-sequence hash,
per-call gate log, verification, truncated answer). Recording is off by
default — growth never happens unless asked for — and the last
--growth/--no-growth wins.";

/// Parsed `amparo run` flags.
#[derive(Debug, Clone)]
pub struct RunFlags {
    pub policy_url: Option<String>,
    pub allow_all: bool,
    pub auto_approve: bool,
    pub auto_deny: bool,
    pub trust_ceiling: ToolTrustTier,
    pub max_steps: Option<usize>,
    pub model: Option<String>,
    pub timeout_secs: Option<u64>,
    pub workspace: Option<String>,
    /// Record PII-stripped run records to the workspace notebook.
    pub growth: bool,
    /// The task — joined positional arguments.
    pub task: String,
}

impl Default for RunFlags {
    fn default() -> Self {
        Self {
            policy_url: None,
            allow_all: false,
            auto_approve: false,
            auto_deny: false,
            trust_ceiling: ToolTrustTier::SystemControl,
            max_steps: None,
            model: None,
            timeout_secs: None,
            workspace: None,
            growth: false,
            task: String::new(),
        }
    }
}

/// Outcome of [`parse_run_flags`]: run with these flags, print
/// [`RUN_USAGE`] and exit 0, or print the message to stderr and exit 2.
#[derive(Debug)]
pub enum ParseRunResult {
    Run(RunFlags),
    Help,
    Error(String),
}

/// Parse `amparo run` flags. Never panics and never exits — problems come
/// back as [`ParseRunResult::Error`].
pub fn parse_run_flags(args: impl Iterator<Item = String>) -> ParseRunResult {
    let mut flags = RunFlags::default();
    let mut positional: Vec<String> = Vec::new();

    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--policy-url" => match args.next() {
                Some(url) => flags.policy_url = Some(url),
                None => return ParseRunResult::Error("--policy-url requires a URL".into()),
            },
            "--allow-all" => flags.allow_all = true,
            "--auto-approve" => flags.auto_approve = true,
            "--auto-deny" => flags.auto_deny = true,
            "--growth" => flags.growth = true,
            "--no-growth" => flags.growth = false,
            "--trust-ceiling" => match args.next() {
                Some(tier) => match tier.as_str() {
                    "observational" => flags.trust_ceiling = ToolTrustTier::Observational,
                    "local_mutating" => flags.trust_ceiling = ToolTrustTier::LocalMutating,
                    "external_effector" => flags.trust_ceiling = ToolTrustTier::ExternalEffector,
                    "system_control" => flags.trust_ceiling = ToolTrustTier::SystemControl,
                    other => {
                        return ParseRunResult::Error(format!("unknown trust tier {other}"))
                    }
                },
                None => {
                    return ParseRunResult::Error("--trust-ceiling requires a tier".into())
                }
            },
            "--max-steps" => match args.next() {
                Some(n) => match n.parse::<usize>() {
                    Ok(steps) if steps > 0 => flags.max_steps = Some(steps),
                    _ => {
                        return ParseRunResult::Error(format!(
                            "--max-steps must be a positive integer, got '{n}'"
                        ))
                    }
                },
                None => {
                    return ParseRunResult::Error("--max-steps requires a number".into())
                }
            },
            "--model" => match args.next() {
                Some(model) => flags.model = Some(model),
                None => return ParseRunResult::Error("--model requires a model ID".into()),
            },
            "--timeout" => match args.next() {
                Some(secs) => match secs.parse::<u64>() {
                    Ok(s) => flags.timeout_secs = Some(s),
                    Err(_) => {
                        return ParseRunResult::Error(format!(
                            "--timeout must be an integer of seconds, got '{secs}'"
                        ))
                    }
                },
                None => return ParseRunResult::Error("--timeout requires seconds".into()),
            },
            "--workspace" => match args.next() {
                Some(dir) => flags.workspace = Some(dir),
                None => return ParseRunResult::Error("--workspace requires a directory".into()),
            },
            "--help" | "-h" => return ParseRunResult::Help,
            other if other.starts_with('-') => {
                return ParseRunResult::Error(format!(
                    "unknown flag {other}; see `amparo run --help`"
                ))
            }
            other => positional.push(other.to_string()),
        }
    }

    if flags.policy_url.is_some() && flags.allow_all {
        return ParseRunResult::Error(
            "--policy-url and --allow-all are mutually exclusive".into(),
        );
    }
    if flags.auto_approve && flags.auto_deny {
        return ParseRunResult::Error(
            "--auto-approve and --auto-deny are mutually exclusive".into(),
        );
    }
    if positional.is_empty() {
        return ParseRunResult::Error(
            "amparo run requires a task (e.g. amparo run \"list the files\")".into(),
        );
    }
    flags.task = positional.join(" ");
    ParseRunResult::Run(flags)
}

/// Entry point for `amparo run`: parse, then execute with exit codes
/// (2 = flag problem, 1 = inference/task failure).
pub async fn dispatch(args: impl Iterator<Item = String>) {
    match parse_run_flags(args) {
        ParseRunResult::Help => println!("{RUN_USAGE}"),
        ParseRunResult::Error(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
        ParseRunResult::Run(flags) => {
            if let Err(message) = execute(flags).await {
                eprintln!("amparo run: {message}");
                std::process::exit(1);
            }
        }
    }
}

/// Wire the gate chain and run the loop. `Err` is a runtime failure (exit 1).
pub async fn execute(flags: RunFlags) -> Result<(), String> {
    let mut config = InferenceConfig::from_env().map_err(|e| {
        format!(
            "{e}\nset AMPARO_INFERENCE_URL and AMPARO_INFERENCE_MODEL — see the README \
             Quickstart for the full environment surface"
        )
    })?;
    if let Some(secs) = flags.timeout_secs {
        config.timeout_secs = secs.clamp(1, MAX_TIMEOUT_SECS);
    }
    if let Some(model) = &flags.model {
        config.model = model.clone();
    }
    let provider = config.build().map_err(|e| e.to_string())?;

    if let Some(dir) = &flags.workspace {
        std::env::set_var("AMPARO_WORKSPACE", dir);
    }
    let registry = default_registry();

    let policy: Arc<dyn PolicyEngine> = match (&flags.policy_url, flags.allow_all) {
        (Some(url), false) => {
            let api_key = std::env::var("AMPARO_POLICY_KEY").ok();
            Arc::new(WirePolicyEngine::new(url.clone(), api_key))
        }
        (None, true) => Arc::new(AllowAllPolicyEngine),
        (None, false) => Arc::new(DenyAllPolicyEngine::new(
            "no policy configured (--policy-url or --allow-all)",
        )),
        (Some(_), true) => unreachable!("rejected by parse_run_flags"),
    };

    let approval: Arc<dyn ApprovalGate> = if flags.auto_approve {
        Arc::new(AutoApprove)
    } else if flags.auto_deny {
        Arc::new(AutoDeny)
    } else {
        Arc::new(InteractiveApprovalGate::default())
    };

    let mut agent_config = AgentConfig::default();
    if let Some(steps) = flags.max_steps {
        agent_config.max_steps = steps;
    }
    agent_config.trust_ceiling = flags.trust_ceiling;
    agent_config.model = flags.model.clone();

    // The lab notebook: with --growth, records flow to a local append-only
    // store next to the printing sink; without it, printing exactly as
    // before. Kept outside the fanout so `flush` can await the final write.
    let printing = Arc::new(PrintingSink);
    let notebook: Option<Arc<NotebookSink>> = if flags.growth {
        let path = PathPolicy::from_env()
            .workspace_root
            .join(".amparo/notebook/records.jsonl");
        let store = JsonlStore::open(&path)
            .map_err(|e| format!("cannot open the growth notebook: {e}"))?;
        eprintln!("[growth] recording PII-stripped run records to {}", path.display());
        Some(Arc::new(NotebookSink::new(Arc::new(store), "cli")))
    } else {
        None
    };
    let sink: Arc<dyn EventSink> = match &notebook {
        Some(nb) => Arc::new(FanoutSink::new(vec![
            printing,
            Arc::clone(nb) as Arc<dyn EventSink>,
        ])),
        None => printing,
    };

    let agent = Agent::new(provider, registry, policy)
        .with_approval(approval)
        .with_events(sink)
        .with_privacy(Arc::new(amparo_privacy::PrivacyPolicy::default()))
        .with_config(agent_config);

    let report = agent.run(flags.task).await;
    // The CLI is a short-lived host: await the pending record write so it
    // cannot lose the race with process exit. Failed tasks write records too.
    if let Some(nb) = &notebook {
        nb.flush().await;
    }
    match report.status {
        TaskStatus::Complete => {
            // stdout = the final answer, nothing else (scripting contract).
            println!("{}", report.final_answer.unwrap_or_default());
            eprintln!(
                "[report] complete — {} step(s), verification: {}",
                report.steps_used,
                report
                    .verification
                    .map(|v| v.decision)
                    .unwrap_or_else(|| "n/a".into())
            );
            Ok(())
        }
        TaskStatus::Failed => {
            eprintln!("[report] failed — {} step(s)", report.steps_used);
            Err(format!(
                "task failed: {}",
                report.final_answer.unwrap_or_else(|| "no final answer".into())
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> ParseRunResult {
        parse_run_flags(args.iter().map(|s| s.to_string()))
    }

    fn flags(result: ParseRunResult) -> RunFlags {
        match result {
            ParseRunResult::Run(f) => f,
            ParseRunResult::Help => panic!("expected Run, got Help"),
            ParseRunResult::Error(m) => panic!("expected Run, got error: {m}"),
        }
    }

    fn error(result: ParseRunResult) -> String {
        match result {
            ParseRunResult::Error(m) => m,
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn task_is_the_joined_positionals() {
        let f = flags(parse(&["say", "hello", "world"]));
        assert_eq!(f.task, "say hello world");
    }

    #[test]
    fn parses_every_flag_with_defaults_for_the_rest() {
        let f = flags(parse(&[
            "--policy-url", "http://policy.test",
            "--auto-approve",
            "--trust-ceiling", "observational",
            "--max-steps", "5",
            "--model", "qwen2.5:14b",
            "--timeout", "30",
            "--workspace", "/tmp/ws",
            "--growth",
            "task",
        ]));
        assert_eq!(f.policy_url.as_deref(), Some("http://policy.test"));
        assert!(f.auto_approve);
        assert!(!f.auto_deny);
        assert!(!f.allow_all);
        assert_eq!(f.trust_ceiling, ToolTrustTier::Observational);
        assert_eq!(f.max_steps, Some(5));
        assert_eq!(f.model.as_deref(), Some("qwen2.5:14b"));
        assert_eq!(f.timeout_secs, Some(30));
        assert_eq!(f.workspace.as_deref(), Some("/tmp/ws"));
        assert!(f.growth);
        assert_eq!(f.task, "task");
    }

    #[test]
    fn help_is_recognized() {
        assert!(matches!(parse(&["--help"]), ParseRunResult::Help));
        assert!(matches!(parse(&["-h"]), ParseRunResult::Help));
    }

    #[test]
    fn rejects_unknown_flags_missing_values_and_bad_numbers() {
        assert_eq!(
            error(parse(&["--nonsense", "task"])),
            "unknown flag --nonsense; see `amparo run --help`"
        );
        assert_eq!(error(parse(&["--policy-url"])), "--policy-url requires a URL");
        assert_eq!(error(parse(&["--trust-ceiling"])), "--trust-ceiling requires a tier");
        assert_eq!(error(parse(&["--trust-ceiling", "nonsense", "task"])), "unknown trust tier nonsense");
        assert_eq!(
            error(parse(&["--max-steps", "zero", "task"])),
            "--max-steps must be a positive integer, got 'zero'"
        );
        assert_eq!(
            error(parse(&["--max-steps", "0", "task"])),
            "--max-steps must be a positive integer, got '0'"
        );
        assert_eq!(
            error(parse(&["--timeout", "soon", "task"])),
            "--timeout must be an integer of seconds, got 'soon'"
        );
    }

    #[test]
    fn rejects_conflicting_modes_and_a_missing_task() {
        assert_eq!(
            error(parse(&["--policy-url", "http://p.test", "--allow-all", "task"])),
            "--policy-url and --allow-all are mutually exclusive"
        );
        assert_eq!(
            error(parse(&["--auto-approve", "--auto-deny", "task"])),
            "--auto-approve and --auto-deny are mutually exclusive"
        );
        assert_eq!(
            error(parse(&["--allow-all"])),
            "amparo run requires a task (e.g. amparo run \"list the files\")"
        );
    }

    #[test]
    fn default_flags_are_deny_by_default() {
        let f = flags(parse(&["task"]));
        assert!(f.policy_url.is_none());
        assert!(!f.allow_all);
        assert!(!f.auto_approve);
        assert!(!f.auto_deny);
        assert!(!f.growth);
        assert_eq!(f.trust_ceiling, ToolTrustTier::SystemControl);
    }

    #[test]
    fn growth_flag_last_wins_and_defaults_off() {
        assert!(!flags(parse(&["task"])).growth);
        assert!(flags(parse(&["--growth", "task"])).growth);
        assert!(!flags(parse(&["--growth", "--no-growth", "task"])).growth);
        assert!(flags(parse(&["--no-growth", "--growth", "task"])).growth);
    }
}
