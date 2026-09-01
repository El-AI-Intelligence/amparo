//! The `amparo run` subcommand: parse flags, wire the gate chain, run the loop.
//!
//! Wiring order matters and is deliberate:
//!
//! 1. [`InferenceConfig::from_env`] fails closed — no silent localhost
//!    defaults. `--timeout` clamps to the same 1–3600s bounds as the env
//!    surface.
//! 2. `AMPARO_WORKSPACE` is set **before**
//!    [`default_registry_with_memory`] — tools
//!    capture the path policy at construction.
//! 3. The policy match is identical to `amparo_mcp::serve`: wire engine,
//!    explicit allow-all, or deny-all with the same reason string.
//! 4. Approval defaults to the interactive terminal gate;
//!    `--auto-approve`/`--auto-deny` never construct it, so a piped
//!    `</dev/null` agent cannot hang on stdin.
//!
//! stdout carries the final answer only; the report goes to stderr.

use amparo_agent::{
    format_cost_line, valid_approval_endpoint, Agent, AgentConfig, AgentReport, ApprovalGate,
    AutoApprove, AutoDeny, CaseLibrary, CheckpointStore, EventSink, FanoutSink,
    JsonCheckpointStore, LedgerSink, SpawnAgentTool, TaskStatus, WebApprovalGate,
};
use amparo_chat::{
    due_scan, schedule_dir, JsonScheduleStore, ScheduleStore, ScheduleTool, ScheduledStatus,
    ScheduledTask, SCHEDULE_GRACE,
};
use amparo_inference::{InferenceConfig, InferenceProvider, MAX_TIMEOUT_SECS};
use amparo_notebook::{
    append_event, auto_rollup, check_skill_drift, notebook_dir, skills_dir, CaseRetriever,
    JsonlStore, NotebookSink, SkillLogEvent, SkillSet, HOT_FILE,
};
use amparo_policy::{
    wire::WirePolicyEngine, AllowAllPolicyEngine, AuditNoticeEngine, DenyAllPolicyEngine,
    PolicyEngine,
};
use amparo_privacy::{privacy_dir, LedgerQuota, LedgerStore};
use amparo_sandbox::EvalWasmTool;
use amparo_tools::{
    default_registry_with_memory, resolve_memory_backend, Memory, PathPolicy, SendNotificationTool,
    SkillLibrary, ToolRegistry, ToolTrustTier, UseSkillTool, DEFAULT_ENGRAM_URL,
};
use std::sync::{Arc, Mutex};

use crate::approve::InteractiveApprovalGate;
use crate::events::PrintingSink;

pub const RUN_USAGE: &str = "\
amparo run — drive the agent loop end-to-end

USAGE:
  amparo run [FLAGS] \"task\"

FLAGS:
  --policy-url URL    wire a remote policy engine (deny-all default)
  --session-id ID     tag every policy check with ID (engine-side audit
                      rows); defaults to the task id
  --allow-all         run without policy checks (explicit opt-in)
  --auto-approve      approve escalated/external-effector calls without a human
  --auto-deny         deny escalated/external-effector calls without asking
  --growth            record PII-stripped run records (off by default)
  --no-growth         never record (overrides an earlier --growth)
  --trust-ceiling T   observational | local_mutating |
                      external_effector | system_control (default)
  --max-steps N       maximum loop iterations (default 12)
  --max-sub-agents N  swarm budget: at most N sub-agents per task
                      (default 4; 0 turns spawn_agent off)
  --model M           override AMPARO_INFERENCE_MODEL for this run
  --timeout SECS      per-request timeout in seconds (clamped 1-3600)
  --workspace DIR     working directory the tools are confined to
                      (sets AMPARO_WORKSPACE)
  --resume            resume the newest incomplete checkpoint (tenant cli);
                      takes no task — the prompt comes from the checkpoint
  --ledger-max-bytes N  bound the always-on privacy ledger file; when it
                      would grow past N, the oldest rows rotate off and a
                      marker records the drop (K/M/G suffixes, e.g. 64K)
  --webhook-url URL   deliver send_notification messages by POSTing them
                      as JSON to URL (default: stderr)
  --approval-endpoint URL  ask a web UI for approvals (60s fail-closed);
                      replaces the interactive prompt

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
--growth/--no-growth wins.

--resume continues a crashed run from <workspace>/.amparo/sessions/cli:
the prompt comes from the checkpoint, and the loop re-judges every tool
call through the current flags' gate chain. A checkpoint older than 7
days is skipped (never resumed) — its gate decisions are too old to
trust.

--max-sub-agents bounds the swarm (M8): the parent registers spawn_agent,
every spawn asks for approval like any external-effector call, and the
report states what the swarm burned — the sub-agent count and chain ids,
the tool-call total and the cost estimate with its method attached.

--session-id carries provenance to the policy engine (M9 W3): every check
in this run is tagged with the id, so engine-side audit rows correlate
with the run — the same seam the chat driver uses for platform:user_id.
Without the flag the task id is the session id (on --resume, the
checkpoint's original task id). The policy engine's audit-mode notice —
\"policy engine is in audit mode; verdicts are advisory\" — prints to
stderr the first time an audit-only verdict comes back, exactly once.

--webhook-url wires the send_notification tool (M10 W2): each
notification is POSTed as {\"destination\", \"message\"} JSON to the URL,
and a non-success status fails the call. Without the flag the default
stderr transport stands — the notification prints as
\"[notification] to <destination>: <message>\". Every send asks for
human approval first (external-effector tier), and the approval prompt
names the destination.

--approval-endpoint wires the human-approval gate to a web UI (M10 W4):
each escalated/external-effector request is POSTed to the URL as
{\"call_id\", \"tool_name\", \"arguments\", \"reasons\", \"blast_radius\",
\"session_label\", \"rollback\"} JSON, and the gate polls URL/<call_id>
for {\"status\":\"decided\",\"decision\":true|false} under a 60-second
deadline — no decision means denial (fail closed). Mutually exclusive
with --auto-approve and --auto-deny.

The schedule tool is always on in `amparo run` (M8 W5 + M10 W5): a
promise persists at <workspace>/.amparo/schedule/ and re-enters the same
gate chain at the next run start. The CLI is process-scoped — no
background ticker — so due promises fire at run start only
(best-effort); a promise whose instant passed beyond the 60-second grace
window is marked missed, never fired late. The scan fires only the
promises the CLI itself wrote (I2); promises made in chat belong to the
chat host's ticker. A fire is a fresh task with a reduced tool set — no
spawn_agent, no schedule: unattended spawn chains would break the
attribution chain.";

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
    /// Resume the newest incomplete checkpoint instead of running a task.
    pub resume: bool,
    /// Bound the privacy ledger file in bytes; when it would grow past
    /// this, the oldest rows rotate off (M7b). `None` = unbounded.
    pub ledger_max_bytes: Option<u64>,
    /// Swarm budget (M8): at most this many sub-agents per task.
    /// `0` turns `spawn_agent` off.
    pub max_sub_agents: usize,
    /// Session id attached to every policy check (M9 W3): correlates
    /// engine-side audit rows with this run. `None` = the task id.
    pub session_id: Option<String>,
    /// Webhook URL for `send_notification` (M10 W2): when set, each
    /// notification is POSTed there as JSON; `None` = the stderr
    /// transport.
    pub webhook_url: Option<String>,
    /// Web-approval endpoint (M10 W4): when set, approval requests POST
    /// to this URL and the gate polls it for the human's decision.
    /// Mutually exclusive with `--auto-approve`/`--auto-deny`.
    pub approval_endpoint: Option<String>,
    /// The task — joined positional arguments (empty when resuming).
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
            resume: false,
            ledger_max_bytes: None,
            max_sub_agents: 4,
            session_id: None,
            webhook_url: None,
            approval_endpoint: None,
            task: String::new(),
        }
    }
}

/// Parse a byte size for `--ledger-max-bytes`: plain digits, or digits
/// plus a `K`/`M`/`G` suffix (powers of 1024, case-insensitive). Rejects
/// zero, negatives, fractions, unknown suffixes and overflow.
pub fn parse_bytes(raw: &str) -> Result<u64, String> {
    let (digits, multiplier) = match raw.as_bytes().last().copied() {
        Some(suffix @ (b'K' | b'M' | b'G' | b'k' | b'm' | b'g')) => {
            let power = match suffix.to_ascii_uppercase() {
                b'K' => 10u32,
                b'M' => 20,
                _ => 30,
            };
            (&raw[..raw.len() - 1], 1u64 << power)
        }
        Some(b'0'..=b'9') => (raw, 1),
        _ => {
            return Err(format!(
                "--ledger-max-bytes must be a positive size like 64K, got '{raw}'"
            ))
        }
    };
    let base: u64 = if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "--ledger-max-bytes must be a positive size like 64K, got '{raw}'"
        ));
    } else {
        digits
            .parse()
            .map_err(|_| format!("--ledger-max-bytes is too large, got '{raw}'"))?
    };
    if base == 0 {
        return Err(format!("--ledger-max-bytes must be positive, got '{raw}'"));
    }
    base.checked_mul(multiplier)
        .ok_or_else(|| format!("--ledger-max-bytes is too large, got '{raw}'"))
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
            "--session-id" => match args.next() {
                Some(id) => flags.session_id = Some(id),
                None => return ParseRunResult::Error("--session-id requires an id".into()),
            },
            "--webhook-url" => match args.next() {
                Some(url) => flags.webhook_url = Some(url),
                None => return ParseRunResult::Error("--webhook-url requires a URL".into()),
            },
            "--approval-endpoint" => match args.next() {
                Some(url) => flags.approval_endpoint = Some(url),
                None => return ParseRunResult::Error("--approval-endpoint requires a URL".into()),
            },
            "--allow-all" => flags.allow_all = true,
            "--auto-approve" => flags.auto_approve = true,
            "--auto-deny" => flags.auto_deny = true,
            "--growth" => flags.growth = true,
            "--no-growth" => flags.growth = false,
            "--resume" => flags.resume = true,
            "--trust-ceiling" => match args.next() {
                Some(tier) => match tier.as_str() {
                    "observational" => flags.trust_ceiling = ToolTrustTier::Observational,
                    "local_mutating" => flags.trust_ceiling = ToolTrustTier::LocalMutating,
                    "external_effector" => flags.trust_ceiling = ToolTrustTier::ExternalEffector,
                    "system_control" => flags.trust_ceiling = ToolTrustTier::SystemControl,
                    other => return ParseRunResult::Error(format!("unknown trust tier {other}")),
                },
                None => return ParseRunResult::Error("--trust-ceiling requires a tier".into()),
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
                None => return ParseRunResult::Error("--max-steps requires a number".into()),
            },
            "--max-sub-agents" => match args.next() {
                // `0` is allowed — it turns swarms off; anything that
                // is not a non-negative integer is a usage error (exit 2).
                Some(n) => match n.parse::<usize>() {
                    Ok(max) => flags.max_sub_agents = max,
                    Err(_) => {
                        return ParseRunResult::Error(format!(
                            "--max-sub-agents must be a non-negative integer, got '{n}'"
                        ))
                    }
                },
                None => return ParseRunResult::Error("--max-sub-agents requires a number".into()),
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
            "--ledger-max-bytes" => match args.next() {
                Some(raw) => match parse_bytes(&raw) {
                    Ok(bytes) => flags.ledger_max_bytes = Some(bytes),
                    Err(message) => return ParseRunResult::Error(message),
                },
                None => return ParseRunResult::Error("--ledger-max-bytes requires a size".into()),
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
        return ParseRunResult::Error("--policy-url and --allow-all are mutually exclusive".into());
    }
    if flags.auto_approve && flags.auto_deny {
        return ParseRunResult::Error(
            "--auto-approve and --auto-deny are mutually exclusive".into(),
        );
    }
    if flags.approval_endpoint.is_some() && (flags.auto_approve || flags.auto_deny) {
        return ParseRunResult::Error(
            "--approval-endpoint and --auto-approve/--auto-deny are mutually exclusive".into(),
        );
    }
    if let Some(url) = &flags.approval_endpoint {
        if !valid_approval_endpoint(url) {
            return ParseRunResult::Error(format!(
                "--approval-endpoint must be an http(s) URL with a host, got '{url}'"
            ));
        }
    }
    if flags.resume && !positional.is_empty() {
        return ParseRunResult::Error(
            "`--resume` takes no task — the prompt comes from the checkpoint".into(),
        );
    }
    if !flags.resume && positional.is_empty() {
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

/// Run a fresh task or resume a checkpoint. `Err` is a runtime failure
/// (exit 1); parse problems exit 2 in [`dispatch`].
pub async fn execute(flags: RunFlags) -> Result<(), String> {
    if flags.resume {
        return execute_resume(&flags).await;
    }
    // AMPARO_WORKSPACE must be set before the tools capture the path
    // policy at construction — see [`wire`].
    apply_workspace(&flags);
    let mut wired = wire(&flags, new_task_id()).await?;
    let report = wired.agent.run(flags.task.clone()).await;
    await_schedule_fires(&mut wired).await;
    finish(wired.notebook, wired.spawn_tool, wired.cost_rate, report).await
}

/// Wait for the run-start schedule fires (M10 W5): they ran concurrently
/// with the main task, and the CLI is a short-lived host — awaiting them
/// here guarantees every promise record is written before the process
/// exits (the same race the notebook flush guards).
async fn await_schedule_fires(wired: &mut WiredRun) {
    for fire in wired.schedule_fires.drain(..) {
        // A JoinError is absorbed — the promise stays pending and the
        // next run start re-scans it.
        let _ = fire.await;
    }
}

/// Apply `--workspace` to the process env before anything reads it: the
/// tools capture `AMPARO_WORKSPACE` when the registry is built, and the
/// checkpoint store roots at the same workspace — both must see the flag.
pub(crate) fn apply_workspace(flags: &RunFlags) {
    if let Some(dir) = &flags.workspace {
        std::env::set_var("AMPARO_WORKSPACE", dir);
    }
}

/// How long a `Running` checkpoint stays resumable. A crash whose
/// checkpoint is older than this is abandoned, never resumed: its gate
/// decisions and claims are too old to trust.
const STALE_CHECKPOINT_SECS: u64 = 7 * 24 * 60 * 60;

/// The `--resume` path: no task on the command line — the prompt comes
/// from the newest `Running` checkpoint for tenant `cli` under the
/// workspace. The resumed loop re-judges every tool call through the
/// current flags' gate chain, exactly like a fresh run.
async fn execute_resume(flags: &RunFlags) -> Result<(), String> {
    apply_workspace(flags);
    let workspace_root = PathPolicy::from_env().workspace_root;
    let store = JsonCheckpointStore::new(&workspace_root);
    let Some(checkpoint) = store.latest_incomplete("cli") else {
        return Err(format!(
            "no incomplete checkpoint for tenant cli under {} — nothing to resume",
            workspace_root.display()
        ));
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let age_secs = now.saturating_sub(checkpoint.started_at);
    if age_secs > STALE_CHECKPOINT_SECS {
        eprintln!(
            "[session] skipping stale running checkpoint {} (started {} day(s) ago) — run a fresh task",
            checkpoint.task_id,
            age_secs / 86_400
        );
        return Err("no resumable checkpoint".into());
    }
    // `wire` attaches the checkpoint store (both paths — a resumed task
    // writes its terminal snapshot through the same store). The resumed
    // task id is the parent id for the spawn tool: children chain off
    // the same id the checkpoint stores.
    let mut wired = wire(flags, checkpoint.task_id.clone()).await?;
    let report = wired.agent.resume(checkpoint).await;
    // A resumed run is a run start too: the schedule scan in `wire` has
    // already spawned its fires — await them before exit.
    await_schedule_fires(&mut wired).await;
    finish(wired.notebook, wired.spawn_tool, wired.cost_rate, report).await
}

/// The task id for a fresh run — the same `sess-<nanos>-<pid>` shape the
/// agent generates when the host names none, so a parent's children
/// chain as `{parent}.{n}` off the same id the checkpoint stores.
pub(crate) fn new_task_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("sess-{nanos}-{}", std::process::id())
}

/// Everything the loop needs once the flags are parsed: the built agent,
/// plus the growth notebook sink that must be flushed after the task,
/// the swarm tool (when the budget is above zero) for the terminal
/// report, and the cost rate the report lines use.
pub(crate) struct WiredRun {
    pub(crate) agent: Agent,
    pub(crate) notebook: Option<Arc<NotebookSink>>,
    pub(crate) spawn_tool: Option<Arc<SpawnAgentTool>>,
    pub(crate) cost_rate: Option<f64>,
    /// The run-start schedule fires (M10 W5): due promises run
    /// concurrently with the main task; the handles are awaited before
    /// the process exits so every promise record lands.
    pub(crate) schedule_fires: Vec<tokio::task::JoinHandle<()>>,
    /// The gate-chain facts the banner rendered (TUI #166): the TUI
    /// re-renders `[chain]` from these when the live state changes
    /// (policy audit flip, memory degrade).
    pub(crate) banner: BannerInfo,
    /// The resolved policy engine — the TUI reads [`PolicyEngine::audit_mode`]
    /// for the status line's `§` symbol.
    pub(crate) policy: Arc<dyn PolicyEngine>,
    /// The resolved memory backend — the TUI reads [`Memory::name`] for
    /// the status line's memory segment.
    pub(crate) memory: Arc<dyn Memory>,
}

/// Reduce a URL to the ledger's `scheme://host[:port]` shape for a
/// status line. Userinfo (`user:pass@`) never survives
/// [`amparo_privacy::ledger::site_host_only`], and it must never survive
/// into stderr either — a key can ride exactly there by mistake.
pub(crate) fn site_desc(url: &str) -> String {
    amparo_privacy::ledger::site_host_only(url).unwrap_or_else(|| "(unparseable url)".to_string())
}

/// The flag spelling for a trust ceiling — the same literals
/// `parse_run_flags` accepts, so the banner reads in the CLI's own
/// vocabulary.
fn tier_name(tier: ToolTrustTier) -> &'static str {
    match tier {
        ToolTrustTier::Observational => "observational",
        ToolTrustTier::Network => "network",
        ToolTrustTier::LocalMutating => "local_mutating",
        ToolTrustTier::ExternalEffector => "external_effector",
        ToolTrustTier::SystemControl => "system_control",
    }
}

/// The gate-chain facts the boot banner names (#165, extended for the
/// TUI #166): everything a surface needs to draw the chain in its own
/// grammar. `amparo run` prints the one-line [`BannerInfo::chain`]; the
/// TUI draws the full `◆──▲──§──◉` diagram and its `[key]`/`[chain]`
/// lines from the same facts.
#[derive(Clone)]
pub struct BannerInfo {
    /// The chain in one line — `registry → trust ceiling (…) → policy
    /// (…) → human approval (…)`.
    pub chain: String,
    /// How many tools are registered — the `◆ registry: N tools` count.
    pub tool_count: usize,
    /// The ceiling's flag spelling (`observational` … `system_control`).
    pub ceiling: String,
    /// The policy's one-line status (`wire https://…`, `deny-all (…)`,
    /// `allow-all`).
    pub policy: String,
    /// The approval's one-line status (`terminal y/N, 60s fail-closed`,
    /// `web …`, `auto-…`).
    pub approval: String,
    /// The resolved provider · model · host — the host in the ledger's
    /// `scheme://host[:port]` shape, never a key-carrying URL.
    pub infer: String,
    /// The resolved memory backend (`built-in store`, `engram @ …`).
    pub memory: String,
}

/// The rendering seam for a wired run (TUI #166). [`wire`] builds a run
/// on the default terminal surface — stderr `[tag]` lines and the
/// interactive terminal approval gate; the TUI calls [`wire_with`] with
/// its own surface: same registry, same policy, same fail-closed chain,
/// rendered in its own grammar.
#[derive(Clone)]
pub struct Surface {
    /// Every loop event renders through this sink (default: the stderr
    /// printing sink).
    pub events: Arc<dyn EventSink>,
    /// The human-approval gate and its one-line status; `None` wires the
    /// gate from the flags exactly like [`wire`] (default).
    pub approval: Option<(Arc<dyn ApprovalGate>, String)>,
    /// Renders the boot banner — called once per wired run (fresh or
    /// resumed).
    pub banner: fn(&BannerInfo),
    /// Renders one `[tag]` status line (`[growth]`, `[schedule]`,
    /// `[ledger]` notices).
    pub line: fn(&str),
    /// Renders the one-time policy audit notice (M9 W3): "policy engine
    /// is in audit mode; verdicts are advisory".
    pub notice: fn(&str),
}

impl Default for Surface {
    fn default() -> Self {
        Self {
            events: Arc::new(PrintingSink),
            approval: None,
            banner: boot_banner,
            line: |line| eprintln!("{line}"),
            notice: |line| eprintln!("{line}"),
        }
    }
}

/// The awakened power-on banner (#165): one stderr block naming who this
/// is and the gate chain the run will enforce. Printed at the end of
/// [`wire`] so a fresh run and a resume share it, and after every degrade
/// decision so each line reports what is actually wired. stderr-only —
/// stdout carries the final answer, and the banner must never touch it.
pub fn boot_banner(info: &BannerInfo) {
    eprintln!("Greetings! My name is Amparo, built by EL AI Intelligence.");
    eprintln!("[wake] Amparo is awake.");
    eprintln!("[gate] chain: {}", info.chain);
    eprintln!("[infer] {}", info.infer);
    eprintln!("[memory] {}", info.memory);
}

/// Wire the gate chain from flags: provider, policy, approval, sinks
/// (printing + optional growth notebook + always-on privacy ledger) and
/// the agent. Shared by a fresh run and a resume — a resumed task is the
/// same loop and the same gates; only the starting state differs.
/// `parent_task_id` names the task (fresh: [`new_task_id`]; resume: the
/// checkpoint's id), so children chain as `{parent}.{n}`.
///
/// The default surface — stderr lines and the interactive terminal gate.
/// The TUI calls [`wire_with`] for the same chain on its own surface.
async fn wire(flags: &RunFlags, parent_task_id: String) -> Result<WiredRun, String> {
    wire_with(flags, parent_task_id, Surface::default()).await
}

/// [`wire`] on an explicit [`Surface`] (TUI #166): the TUI's renderer and
/// approval gate replace the stderr printing sink and the interactive
/// prompt; everything else — the gate chain, the fail-closed semantics,
/// the checkpoint store — is identical.
pub(crate) async fn wire_with(
    flags: &RunFlags,
    parent_task_id: String,
    surface: Surface,
) -> Result<WiredRun, String> {
    // The first-run profile (wizard, #167): load the workspace-local
    // profile and fill any environment gaps before the configs read the
    // environment — env always wins, the profile only fills unset vars.
    // The workspace root resolves once here and is reused below (ledger,
    // notebook, checkpoint store).
    let workspace_root = PathPolicy::from_env().workspace_root;
    let profile = crate::wizard::load(&workspace_root);
    if let Some(profile) = &profile {
        crate::wizard::fill_env_gaps(profile);
    }
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
    // The boot banner's infer line names the resolved provider, model and
    // host — captured after the flags override, before the config moves
    // on. The host is the ledger's `scheme://host[:port]` shape: a
    // key-carrying URL must never reach stderr.
    let infer_desc = format!(
        "{} · {} · {}",
        format!("{:?}", config.provider).to_lowercase(),
        config.model,
        site_desc(&config.base_url)
    );
    let provider = config.build().map_err(|e| e.to_string())?;

    // The memory backend (M11 W1): resolved once per process — the
    // Engram adapter when configured and reachable, the built-in store
    // otherwise (with one `[memory]` warning on the degrade path). The
    // banner names the store that actually resolved, never the one that
    // was merely requested.
    let memory = resolve_memory_backend().await;
    let memory_desc = match memory.name() {
        "engram" => {
            let url = std::env::var("AMPARO_ENGRAM_URL")
                .unwrap_or_else(|_| DEFAULT_ENGRAM_URL.to_string());
            format!("engram @ {}", site_desc(&url))
        }
        _ => "built-in store".to_string(),
    };
    // The base registry (M10 W5): shared with the fire path — the growth
    // layer (skills) is the only thing layered on top for the main task.
    let mut registry = fire_registry(flags, Arc::clone(&memory));

    // Policy-URL fallback (wizard, #167): a profile URL fills a missing
    // --policy-url — but never under --allow-all (that flag explicitly
    // opts out of policy) and never over an explicit flag.
    let policy_from_profile = flags.policy_url.is_none() && !flags.allow_all;
    let policy_url = flags.policy_url.clone().or_else(|| {
        if policy_from_profile {
            profile.as_ref().and_then(|p| p.policy_url.clone())
        } else {
            None
        }
    });

    let (policy, policy_desc): (Arc<dyn PolicyEngine>, String) =
        match (&policy_url, flags.allow_all) {
            (Some(url), false) => {
                let api_key = std::env::var("AMPARO_POLICY_KEY").ok();
                // M9 W3: every check carries the run's session id (the task id
                // by default) so engine-side audit rows correlate with this
                // run; the audit-mode notice prints once, on the first
                // audit-only verdict.
                let session_id = flags
                    .session_id
                    .clone()
                    .unwrap_or_else(|| parent_task_id.clone());
                // The banner marks the URL's provenance: "(profile)" when
                // it came from the first-run profile, nothing when the
                // flag said it outright.
                let desc = if policy_from_profile {
                    format!("wire {} (profile)", site_desc(url))
                } else {
                    format!("wire {}", site_desc(url))
                };
                (
                    Arc::new(AuditNoticeEngine::with_printer(
                        WirePolicyEngine::new(url.clone(), api_key).with_session_id(session_id),
                        Arc::new(std::sync::atomic::AtomicBool::new(false)),
                        surface.notice,
                    )),
                    desc,
                )
            }
            (None, true) => (Arc::new(AllowAllPolicyEngine), "allow-all".to_string()),
            (None, false) => (
                Arc::new(DenyAllPolicyEngine::new(
                    "no policy configured (--policy-url or --allow-all)",
                )),
                "deny-all (no --policy-url or --allow-all)".to_string(),
            ),
            (Some(_), true) => unreachable!("rejected by parse_run_flags"),
        };

    let (approval, approval_desc): (Arc<dyn ApprovalGate>, String) = match &surface.approval {
        // The TUI brings its own gate (single-keypress approval cards) —
        // the same fail-closed contract, its own renderer.
        Some((gate, desc)) => (Arc::clone(gate), desc.clone()),
        None => match (
            &flags.approval_endpoint,
            flags.auto_approve,
            flags.auto_deny,
        ) {
            (Some(url), false, false) => (
                Arc::new(WebApprovalGate::new(url.clone())),
                format!("web {}", site_desc(url)),
            ),
            (None, true, false) => (Arc::new(AutoApprove), "auto-approve".to_string()),
            (None, false, true) => (Arc::new(AutoDeny), "auto-deny".to_string()),
            (None, false, false) => (
                Arc::new(InteractiveApprovalGate::default()),
                "terminal y/N, 60s fail-closed".to_string(),
            ),
            _ => unreachable!("rejected by parse_run_flags"),
        },
    };

    let mut agent_config = AgentConfig::default();
    if let Some(steps) = flags.max_steps {
        agent_config.max_steps = steps;
    }
    agent_config.trust_ceiling = flags.trust_ceiling;
    agent_config.model = flags.model.clone();
    // The report's cost lines (M8 W1 + W4) use the same rate the loop
    // carried — captured before the config moves into the agent.
    let cost_rate = agent_config.cost_per_million_tokens;

    // The lab notebook: with --growth, records flow to a local append-only
    // store next to the printing sink; without it, printing exactly as
    // before. Kept outside the fanout so `flush` can await the final write.
    // The same store feeds the case library (M6b): prior `cli` records are
    // retrieved into the self-verification prompt — growth is write + read.
    // The workspace root (resolved at the top of wire_with, with the
    // profile) anchors the privacy ledger (always-on) and the growth
    // notebook (--growth only).
    let mut case_library: Option<Arc<dyn CaseLibrary>> = None;
    let mut skills: Option<Arc<dyn SkillLibrary>> = None;
    let notebook: Option<Arc<NotebookSink>> = if flags.growth {
        let nb_dir = notebook_dir(&workspace_root);
        let cold_path = nb_dir.join("records.jsonl");
        let store: Arc<dyn amparo_tools::Memory> = Arc::new(
            JsonlStore::open(&cold_path)
                .map_err(|e| format!("cannot open the growth notebook: {e}"))?,
        );
        (surface.line)(&format!(
            "[growth] recording PII-stripped run records to {}",
            cold_path.display()
        ));
        // Rollup and archival (M6e): promote the cold tail into the hot
        // layer — folding it daily — before retrieval, so the case
        // library reads this workspace's hot copy too. Observational: a
        // failure warns and never fails the task.
        match auto_rollup(&nb_dir, chrono::Utc::now()) {
            Ok(Some(report)) if report.promoted > 0 => {
                (surface.line)(&format!(
                    "[growth] notebook: promoted {} tail record(s) to the hot layer",
                    report.promoted
                ));
            }
            Ok(_) => {}
            Err(e) => (surface.line)(&format!("[growth] notebook rollup failed: {e}")),
        }
        // The hot layer is what the case library reads (M6b + M6e): the
        // informative subset — dedupe survivors plus gate events of
        // interest. This store is read-only by convention: the sink owns
        // the cold store, and only the rollup writes hot.
        let hot_path = nb_dir.join(HOT_FILE);
        let hot_store: Arc<dyn amparo_tools::Memory> = Arc::new(
            JsonlStore::open(&hot_path)
                .map_err(|e| format!("cannot open the notebook hot layer: {e}"))?,
        );
        (surface.line)("[growth] retrieval: prior cli cases (hot layer) inform self-verification");
        case_library = Some(Arc::new(CaseRetriever::new(Arc::clone(&hot_store), "cli")));
        // Gated skills (M6c + M6d): adopted skills register `use_skill`
        // and the loop expands it step by step through the gate chain.
        // Startup drift re-check (M6d): a skill whose step plan this
        // task's policy/ceiling/registry would block retires before
        // registration. Growth is observational — a write failure warns
        // and continues, never failing the task. No adopted skills → no
        // registration and no line (byte-stable without growth).
        let skills_path = skills_dir(&workspace_root).join("adopted.jsonl");
        let skill_set = SkillSet::load(&skills_path, "cli");
        let adopted_names = skill_set.names();
        if !adopted_names.is_empty() {
            let now = chrono::Utc::now().to_rfc3339();
            for name in &adopted_names {
                let Some(spec) = skill_set.get(name) else {
                    continue;
                };
                if let Some(reason) =
                    check_skill_drift(&spec, &registry, flags.trust_ceiling, policy.as_ref()).await
                {
                    if let Err(e) = append_event(
                        &skills_path,
                        &SkillLogEvent::retire("cli", name, &now, reason.clone()),
                    ) {
                        (surface.line)(&format!("[growth] retire write failed: {e}"));
                    }
                    (surface.line)(&format!("[growth] skill {name} retired: {reason}"));
                }
            }
            let survivors = SkillSet::load(&skills_path, "cli");
            let survivor_names = survivors.names();
            if !survivor_names.is_empty() {
                let library: Arc<dyn SkillLibrary> = Arc::new(survivors);
                registry.register(Arc::new(UseSkillTool::new(Arc::clone(&library))));
                (surface.line)(&format!(
                    "[growth] skills: {} adopted for tenant cli",
                    survivor_names.len()
                ));
                skills = Some(library);
            }
        }
        Some(Arc::new(NotebookSink::new(store, "cli")))
    } else {
        None
    };
    // The privacy ledger (M7): always-on, independent of --growth — an I6
    // instrument recording every network-tool execution attempt and PII
    // strip, readable via `amparo privacy`. An open failure warns and the
    // run continues without the ledger; a write failure warns once per
    // task inside the sink itself. The parent's task id is the sink's
    // stamp for this task's own rows; a spawned child pushes its own
    // frame off the event stream (M8 W4), so every row names whose call
    // it was.
    let ledger: Option<Arc<LedgerSink>> = match LedgerStore::open_with_quota(
        privacy_dir(&workspace_root).join("ledger.jsonl"),
        flags.ledger_max_bytes.map(LedgerQuota::new),
    ) {
        Ok(store) => Some(Arc::new(LedgerSink::new(
            store,
            "cli",
            Some(parent_task_id.clone()),
            None,
        ))),
        Err(e) => {
            (surface.line)(&format!(
                "[ledger] unavailable — the run continues without the privacy ledger: {e}"
            ));
            None
        }
    };
    let mut sinks: Vec<Arc<dyn EventSink>> = vec![Arc::clone(&surface.events)];
    if let Some(nb) = &notebook {
        sinks.push(Arc::clone(nb) as Arc<dyn EventSink>);
    }
    if let Some(ledger) = &ledger {
        sinks.push(Arc::clone(ledger) as Arc<dyn EventSink>);
    }
    let sink: Arc<dyn EventSink> = if sinks.len() == 1 {
        sinks.pop().expect("at least one sink")
    } else {
        Arc::new(FanoutSink::new(sinks))
    };

    // The registry count the banner reports (TUI #166) — captured before
    // the registry moves into the agent.
    let tool_count = registry.tool_count();

    // The scheduler needs these parts after the agent is built (the fires
    // share them with the main task), so the agent holds clones.
    let mut agent = Agent::new(Arc::clone(&provider), registry, Arc::clone(&policy))
        .with_approval(Arc::clone(&approval))
        .with_events(sink)
        .with_privacy(Arc::new(amparo_privacy::PrivacyPolicy::default()))
        // Preflight (M7): the env-derived path policy (AMPARO_WORKSPACE
        // was applied above) drives the blast-radius label on approval
        // prompts. Display-only.
        .with_path_policy(Arc::new(PathPolicy::from_env()))
        // Session persistence (M7 W6): every run — fresh or resumed —
        // snapshots its loop so a crash can be resumed. The store roots
        // at the same workspace as the ledger and notebook.
        .with_checkpoints(Arc::new(JsonCheckpointStore::new(&workspace_root)), "cli")
        // The host names the task (M8): the checkpoint and the swarm
        // chain share one id, so children chain off the parent's real
        // session id.
        .with_task_id(parent_task_id.clone())
        .with_config(agent_config);
    if let Some(library) = case_library {
        agent = agent.with_case_library(library);
    }
    if let Some(library) = skills {
        agent = agent.with_skills(library);
    }
    // The swarm (M8): with the budget above zero the parent registers
    // `spawn_agent` — the tool captures this agent's parts as they now
    // stand (a registry without itself), and every child runs the same
    // loop under the same gate chain. The shared budget counter fails
    // closed at the limit.
    let spawn_tool = if flags.max_sub_agents > 0 {
        let budget = Arc::new(Mutex::new(flags.max_sub_agents));
        let (swarming, tool) = agent.with_spawn_agent(
            parent_task_id.clone(),
            Arc::clone(&budget),
            flags.max_sub_agents,
        );
        agent = swarming;
        Some(tool)
    } else {
        None
    };
    // The schedule queue (M8 W5 + M10 W5): `amparo run` always registers
    // `schedule` — a promise persists to the workspace queue and fires at
    // the next run start. Registered AFTER `with_spawn_agent` — the spawn
    // tool captured the agent's parts before this call — so children
    // never inherit it: only a top-level task may schedule.
    agent = agent.with_tool(Arc::new(ScheduleTool::for_cli(
        Arc::new(JsonScheduleStore::new(schedule_dir(&workspace_root))),
        parent_task_id.clone(),
    )));

    // The CLI scheduler (M10 W5): the process is short-lived — no ticker
    // — so the queue is scanned once, at run start. Missed promises are
    // marked fail-closed; due ones fire concurrently with the main task
    // (the handles are awaited before the process exits, so every
    // promise record lands).
    let schedule_fires = scan_schedules(
        Arc::clone(&provider),
        Arc::clone(&policy),
        Arc::clone(&approval),
        flags,
        &workspace_root,
        Arc::clone(&memory),
        surface.clone(),
    )
    .await;

    // The awakened power-on experience (#165): every run start — fresh
    // or resumed — greets once with the chain it will enforce, so the
    // one rule is on screen before any tool call exists. The surface's
    // renderer owns the layout: `run` prints the five stderr lines, the
    // TUI draws the full chain diagram from the same facts.
    let banner_info = BannerInfo {
        chain: format!(
            "registry → trust ceiling ({}) → policy ({}) → human approval ({})",
            tier_name(flags.trust_ceiling),
            policy_desc,
            approval_desc,
        ),
        tool_count,
        ceiling: tier_name(flags.trust_ceiling).to_string(),
        policy: policy_desc,
        approval: approval_desc,
        infer: infer_desc,
        memory: memory_desc,
    };
    (surface.banner)(&banner_info);

    Ok(WiredRun {
        agent,
        notebook,
        spawn_tool,
        cost_rate,
        schedule_fires,
        banner: banner_info,
        policy: Arc::clone(&policy),
        memory: Arc::clone(&memory),
    })
}

/// The fire registry (M10 W5): the deliberately reduced tool set a fired
/// promise gets — no `spawn_agent`, no `schedule`: unattended spawn
/// chains would break the attribution chain, and a fire scheduling a
/// fire would defeat the queue's purpose. The main task's registry
/// starts from the same base — the growth layer (skills) is the only
/// thing layered on top for the main task.
fn fire_registry(flags: &RunFlags, memory: Arc<dyn Memory>) -> ToolRegistry {
    let mut registry = default_registry_with_memory(memory);
    // M7b: eval_wasm is host-registered (like use_skill), not part of the
    // default registry — amparo-tools stays wasmtime-free.
    registry.register(Arc::new(EvalWasmTool::new()));
    // M10 W2: with --webhook-url the notification tool POSTs to the
    // webhook; without it the default registry's stderr transport stands.
    if let Some(url) = &flags.webhook_url {
        registry.register(Arc::new(SendNotificationTool::to_webhook(url.clone())));
    }
    registry
}

/// One run-start scan of the schedule queue (M10 W5) — the CLI's whole
/// scheduler. Only `cli` promises are scanned (I2): promises made in
/// chat belong to the chat host's ticker, which resolves the recorded
/// tenant's own parts — the CLI cannot, and never touches them. Missed
/// promises are marked fail-closed with the standard note; due ones
/// fire as fresh top-level tasks through the same provider, policy and
/// approval gate the operator configured for this run. The returned
/// handles are awaited by the caller before the process exits, so every
/// promise record lands.
async fn scan_schedules(
    provider: Arc<dyn InferenceProvider>,
    policy: Arc<dyn PolicyEngine>,
    approval: Arc<dyn ApprovalGate>,
    flags: &RunFlags,
    workspace_root: &std::path::Path,
    memory: Arc<dyn Memory>,
    surface: Surface,
) -> Vec<tokio::task::JoinHandle<()>> {
    let store = Arc::new(JsonScheduleStore::new(schedule_dir(workspace_root)));
    let tasks: Vec<ScheduledTask> = store
        .load_all()
        .into_iter()
        .filter(|task| task.platform == "cli")
        .collect();
    let (due, missed) = due_scan(&tasks, chrono::Utc::now(), SCHEDULE_GRACE);
    for &index in &missed {
        let mut task = tasks[index].clone();
        task.status = ScheduledStatus::Missed;
        task.result = Some(
            "the instant passed and the promise was never fired. Re-schedule it if it \
             still matters."
                .to_string(),
        );
        if let Err(e) = store.save(&task) {
            (surface.line)(&format!("[schedule] cannot mark {} missed: {e}", task.id));
            continue;
        }
        (surface.line)(&format!(
            "[schedule] {} missed — {}",
            task.id,
            task.result.as_deref().unwrap_or_default()
        ));
    }
    let mut handles = Vec::with_capacity(due.len());
    for &index in &due {
        let task = tasks[index].clone();
        let store = Arc::clone(&store);
        let provider = Arc::clone(&provider);
        let policy = Arc::clone(&policy);
        let approval = Arc::clone(&approval);
        let flags = flags.clone();
        let root = workspace_root.to_path_buf();
        let memory = Arc::clone(&memory);
        let surface = surface.clone();
        handles.push(tokio::spawn(async move {
            fire_promise(
                provider, policy, approval, &flags, &root, store, memory, surface, task,
            )
            .await;
        }));
    }
    handles
}

/// Fire one due promise (M10 W5): run the promise's task through the
/// full gate chain — the same provider, policy and approval the operator
/// configured for this run — then record the outcome on the promise and
/// report it on stderr. The fire is a fresh top-level task, deliberately
/// reduced: no spawn_agent, no schedule, no skills, no case library, no
/// continuity. A fire whose task needs approval asks the same gate as
/// the main run (interactive prompt, web endpoint, or an `--auto-*`
/// flag) — a scheduled promise never executes ahead of the gate.
async fn fire_promise(
    provider: Arc<dyn InferenceProvider>,
    policy: Arc<dyn PolicyEngine>,
    approval: Arc<dyn ApprovalGate>,
    flags: &RunFlags,
    workspace_root: &std::path::Path,
    store: Arc<JsonScheduleStore>,
    memory: Arc<dyn Memory>,
    surface: Surface,
    mut task: ScheduledTask,
) {
    let fire_id = new_task_id();
    // The privacy ledger (always-on, like every task): the fire's rows
    // are stamped with its own task id — the promise id stays legible on
    // the promise record itself.
    let ledger: Option<Arc<LedgerSink>> = match LedgerStore::open_with_quota(
        privacy_dir(workspace_root).join("ledger.jsonl"),
        flags.ledger_max_bytes.map(LedgerQuota::new),
    ) {
        Ok(store) => Some(Arc::new(LedgerSink::new(
            store,
            "cli",
            Some(fire_id.clone()),
            None,
        ))),
        Err(e) => {
            (surface.line)(&format!(
                "[ledger] unavailable — the fire continues without the privacy ledger: {e}"
            ));
            None
        }
    };
    let mut sinks: Vec<Arc<dyn EventSink>> = vec![Arc::clone(&surface.events)];
    if let Some(ledger) = &ledger {
        sinks.push(Arc::clone(ledger) as Arc<dyn EventSink>);
    }
    let sink: Arc<dyn EventSink> = if sinks.len() == 1 {
        sinks.pop().expect("at least one sink")
    } else {
        Arc::new(FanoutSink::new(sinks))
    };
    // Checkpoints are written like any task, continuity OFF: the
    // promise names one concrete fresh task.
    let agent = Agent::new(provider, fire_registry(flags, memory), policy)
        .with_events(sink)
        .with_approval(approval)
        .with_privacy(Arc::new(amparo_privacy::PrivacyPolicy::default()))
        .with_path_policy(Arc::new(PathPolicy::from_env()))
        .with_checkpoints(Arc::new(JsonCheckpointStore::new(workspace_root)), "cli")
        .with_task_id(fire_id)
        .with_config(AgentConfig {
            trust_ceiling: flags.trust_ceiling,
            ..AgentConfig::default()
        });
    let report = agent.run(task.task.clone()).await;
    let answer = report
        .final_answer
        .unwrap_or_else(|| "The task failed — no final answer was produced.".to_string());
    task.status = ScheduledStatus::Fired;
    task.result = Some(answer.clone());
    if let Err(e) = store.save(&task) {
        (surface.line)(&format!(
            "[schedule] cannot record the fired promise {}: {e}",
            task.id
        ));
        return;
    }
    (surface.line)(&format!("[schedule] {} fired — {answer}", task.id));
}

/// The shared terminal for a fresh run and a resume: flush the growth
/// notebook — the CLI is a short-lived host, so the pending record write
/// must not lose the race with process exit (failed tasks write records
/// too) — then map the report to stdout/stderr and the exit code.
async fn finish(
    notebook: Option<Arc<NotebookSink>>,
    spawn_tool: Option<Arc<SpawnAgentTool>>,
    cost_rate: Option<f64>,
    report: AgentReport,
) -> Result<(), String> {
    if let Some(nb) = &notebook {
        nb.flush().await;
    }
    // The observatory (M8 W1 + W4): the cost line with its method
    // attached, then the swarm breakdown when sub-agents ran — the
    // parent's own tool calls and tokens included in the total.
    if let Some(line) = format_cost_line(report.tokens_estimated, cost_rate) {
        eprintln!("[report] {line}");
    }
    if let Some(tool) = &spawn_tool {
        let reports = tool.reports();
        if !reports.is_empty() {
            eprintln!(
                "[swarm] {}",
                tool.summary_line(report.tool_calls, report.tokens_estimated, cost_rate)
            );
        }
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
                report
                    .final_answer
                    .unwrap_or_else(|| "no final answer".into())
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amparo_tools::InMemoryStore;

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
            "--policy-url",
            "http://policy.test",
            "--auto-approve",
            "--trust-ceiling",
            "observational",
            "--max-steps",
            "5",
            "--max-sub-agents",
            "7",
            "--model",
            "qwen2.5:14b",
            "--timeout",
            "30",
            "--workspace",
            "/tmp/ws",
            "--growth",
            "task",
        ]));
        assert_eq!(f.policy_url.as_deref(), Some("http://policy.test"));
        assert!(f.auto_approve);
        assert!(!f.auto_deny);
        assert!(!f.allow_all);
        assert_eq!(f.trust_ceiling, ToolTrustTier::Observational);
        assert_eq!(f.max_steps, Some(5));
        assert_eq!(f.max_sub_agents, 7);
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
    fn session_id_parses_defaults_to_none_and_rejects_missing_value() {
        assert_eq!(
            flags(parse(&["--session-id", "web-1", "task"]))
                .session_id
                .as_deref(),
            Some("web-1")
        );
        assert!(flags(parse(&["task"])).session_id.is_none());
        assert_eq!(
            error(parse(&["--session-id"])),
            "--session-id requires an id"
        );
    }

    #[test]
    fn rejects_unknown_flags_missing_values_and_bad_numbers() {
        assert_eq!(
            error(parse(&["--nonsense", "task"])),
            "unknown flag --nonsense; see `amparo run --help`"
        );
        assert_eq!(
            error(parse(&["--policy-url"])),
            "--policy-url requires a URL"
        );
        assert_eq!(
            error(parse(&["--trust-ceiling"])),
            "--trust-ceiling requires a tier"
        );
        assert_eq!(
            error(parse(&["--trust-ceiling", "nonsense", "task"])),
            "unknown trust tier nonsense"
        );
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
            error(parse(&[
                "--policy-url",
                "http://p.test",
                "--allow-all",
                "task"
            ])),
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

    #[test]
    fn resume_parses_without_a_task_and_rejects_one() {
        let f = flags(parse(&["--resume"]));
        assert!(f.resume);
        assert!(f.task.is_empty());
        assert_eq!(
            error(parse(&["--resume", "a task"])),
            "`--resume` takes no task — the prompt comes from the checkpoint"
        );
    }

    #[test]
    fn resume_combines_with_flags_and_defaults_off() {
        assert!(!flags(parse(&["task"])).resume);
        let f = flags(parse(&["--resume", "--max-steps", "3", "--auto-approve"]));
        assert!(f.resume);
        assert_eq!(f.max_steps, Some(3));
        assert!(f.auto_approve);
    }

    #[test]
    fn ledger_max_bytes_parses_plain_and_suffixed_sizes() {
        assert_eq!(parse_bytes("1024"), Ok(1024));
        assert_eq!(parse_bytes("4K"), Ok(4096));
        assert_eq!(parse_bytes("4k"), Ok(4096));
        assert_eq!(parse_bytes("2M"), Ok(2 * 1024 * 1024));
        assert_eq!(parse_bytes("1G"), Ok(1024 * 1024 * 1024));
        assert_eq!(parse_bytes("1g"), Ok(1024 * 1024 * 1024));
        assert_eq!(parse_bytes("18446744073709551615"), Ok(u64::MAX));
    }

    #[test]
    fn ledger_max_bytes_rejects_garbage() {
        for raw in [
            "",
            "K",
            "0",
            "4KB",
            "4.5K",
            "12 K",
            "-1",
            "18446744073709551615G",
            "M3",
        ] {
            assert!(parse_bytes(raw).is_err(), "'{raw}' must be rejected");
        }
    }

    #[test]
    fn ledger_max_bytes_flag_parses_and_errors_exit_2_shaped() {
        assert_eq!(flags(parse(&["task"])).ledger_max_bytes, None);
        let f = flags(parse(&["--ledger-max-bytes", "64K", "task"]));
        assert_eq!(f.ledger_max_bytes, Some(65_536));
        assert!(error(parse(&["--ledger-max-bytes", "banana", "task"])).contains("64K"));
        assert!(error(parse(&["--ledger-max-bytes"])).contains("requires a size"));
    }

    #[test]
    fn max_sub_agents_defaults_to_four_with_zero_disabling_swarms() {
        assert_eq!(flags(parse(&["task"])).max_sub_agents, 4);
        assert_eq!(
            flags(parse(&["--max-sub-agents", "2", "task"])).max_sub_agents,
            2
        );
        assert_eq!(
            flags(parse(&["--max-sub-agents", "0", "task"])).max_sub_agents,
            0
        );
        assert_eq!(
            error(parse(&["--max-sub-agents", "-1", "task"])),
            "--max-sub-agents must be a non-negative integer, got '-1'"
        );
        assert_eq!(
            error(parse(&["--max-sub-agents", "many", "task"])),
            "--max-sub-agents must be a non-negative integer, got 'many'"
        );
        assert_eq!(
            error(parse(&["--max-sub-agents"])),
            "--max-sub-agents requires a number"
        );
    }

    #[test]
    fn approval_endpoint_flag_parses_and_validates() {
        assert_eq!(flags(parse(&["task"])).approval_endpoint, None);
        let f = flags(parse(&[
            "--approval-endpoint",
            "http://web.test/approvals",
            "task",
        ]));
        assert_eq!(
            f.approval_endpoint.as_deref(),
            Some("http://web.test/approvals")
        );
        assert_eq!(
            error(parse(&["--approval-endpoint"])),
            "--approval-endpoint requires a URL"
        );
        assert_eq!(
            error(parse(&["--approval-endpoint", "nonsense", "task"])),
            "--approval-endpoint must be an http(s) URL with a host, got 'nonsense'"
        );
    }

    #[test]
    fn fire_registry_lacks_spawn_and_schedule() {
        let registry = fire_registry(&RunFlags::default(), Arc::new(InMemoryStore::new()));
        assert!(
            registry.get_executor("spawn_agent").is_none(),
            "a fire never spawns — unattended chains break attribution"
        );
        assert!(
            registry.get_executor("schedule").is_none(),
            "a fire never schedules — one fire must not seed the next"
        );
        assert!(
            registry.get_executor("run_command").is_some(),
            "the base tools remain"
        );
        assert!(
            registry.get_executor("eval_wasm").is_some(),
            "the sandbox tool remains"
        );
    }

    #[test]
    fn approval_endpoint_excludes_the_auto_gates() {
        for extra in ["--auto-approve", "--auto-deny"] {
            assert_eq!(
                error(parse(&[
                    "--approval-endpoint",
                    "http://web.test",
                    extra,
                    "task"
                ])),
                "--approval-endpoint and --auto-approve/--auto-deny are mutually exclusive"
            );
        }
    }
}
