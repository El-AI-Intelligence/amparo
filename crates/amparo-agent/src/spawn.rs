//! `spawn_agent` — the M8 swarm tool that builds and runs a child agent.
//!
//! A sub-agent is not an exemption. The child is a fresh [`Agent`] built
//! from the parent's inherited parts: the same inference provider, the same
//! deny-by-default policy engine, the same human-approval gate, the same
//! event sink and the same loop configuration. The spawn call itself is a
//! gated tool call in the parent's loop (tier [`ToolTrustTier::ExternalEffector`],
//! so a human approves every spawn), and every call the child makes runs
//! the child's own full gate chain. Delegation changes only identity and
//! provenance: child ids are `{parent}.{n}`, checkpoints and ledger rows
//! carry the chain, and the approval copy names it.
//!
//! The whole swarm draws from one budget: a shared remaining-spawns
//! counter, decremented per spawn and handed to every child's own spawn
//! tool, so grandchildren count against the same cap. Budget exhaustion
//! fails closed — the result says `swarm budget exhausted: N sub-agents
//! max` and no agent exists.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use amparo_tools::{ToolCall, ToolExecutor, ToolParam, ToolResult, ToolSchema, ToolTrustTier};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::agent::{Agent, AgentConfig, AgentStep, SwarmParts, TaskStatus};
use crate::events::{truncate, AgentEvent};

/// The registry name of the sub-agent tool.
pub const SPAWN_AGENT: &str = "spawn_agent";

/// What one finished child did, in completion order across the swarm —
/// the host's swarm report (M8).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentSummary {
    /// The child's chain id — `{parent}.{n}`.
    pub task_id: String,
    /// The task the child was spawned from.
    pub parent_task_id: String,
    /// How the child ended.
    pub status: TaskStatus,
    /// The child's estimated token consumption, as the cost line reports it.
    pub tokens_estimated: usize,
    /// Tool calls the child requested (blocked ones included).
    pub tool_calls: usize,
    /// The head of the child's final answer, when one exists.
    pub final_answer_head: Option<String>,
}

/// The `spawn_agent` tool. The host constructs one per parent task and
/// registers it on the parent's registry; every spawn builds the child
/// from the captured inherited parts and registers a *child-flavored*
/// instance (the child's id as `parent_task_id`, the same shared budget,
/// counters and reports) on the child's registry, so grandchildren
/// continue the chain under the one budget.
pub struct SpawnAgentTool {
    /// The child's inheritance, captured at construction.
    parts: SwarmParts,
    /// The task id of the agent this tool instance is registered for —
    /// a spawn's child id is `{parent_task_id}.{n}`.
    parent_task_id: String,
    /// Remaining spawns across the whole swarm, shared across generations.
    budget: Arc<Mutex<usize>>,
    /// Per-parent spawn ordinals: `{parent}.{n}` numbering stays monotonic
    /// even when a resumed parent spawns again.
    spawn_counts: Arc<Mutex<HashMap<String, usize>>>,
    /// The swarm cap — named in the exhaustion message.
    max_sub_agents: usize,
    /// Optional ceiling override for children: `None` inherits the
    /// parent's config unchanged.
    trust_ceiling: Option<ToolTrustTier>,
    /// Every completed child, in completion order — read via
    /// [`SpawnAgentTool::reports`].
    reports: Arc<Mutex<Vec<SubAgentSummary>>>,
    /// Completed child runs by call id. The loop retries a failed tool
    /// call up to twice; replaying the cached result instead of re-running
    /// the child keeps a failure from duplicating the child's work — and
    /// its budget spend — twice over.
    done: Mutex<HashMap<String, ToolResult>>,
}

impl SpawnAgentTool {
    /// A spawn tool for the task whose id is `parent_task_id`, inheriting
    /// the parent's provider, gate chain, sink and instruments.
    ///
    /// The child registry root is the parent's own registry as it stands
    /// at construction — **without** this tool registered (hosts attach
    /// the tool afterwards, via [`Agent::with_spawn_agent`]) — so each
    /// child's registry is that spawn-free clone plus a child-flavored
    /// spawn tool, and the tool never holds an `Arc` back to itself.
    ///
    /// `budget` starts at `max_sub_agents` and is consumed by every spawn
    /// in the swarm, across generations; the host can read what remains
    /// through the same `Arc`.
    pub fn new(
        parent: &Agent,
        parent_task_id: impl Into<String>,
        budget: Arc<Mutex<usize>>,
        max_sub_agents: usize,
    ) -> Self {
        Self {
            parts: parent.swarm_parts(parent.registry().clone()),
            parent_task_id: parent_task_id.into(),
            budget,
            spawn_counts: Arc::new(Mutex::new(HashMap::new())),
            max_sub_agents,
            trust_ceiling: None,
            reports: Arc::new(Mutex::new(Vec::new())),
            done: Mutex::new(HashMap::new()),
        }
    }

    /// Cap the children's trust ceiling below the parent's — `Some(tier)`
    /// overrides the inherited config's ceiling for every child. `None`
    /// (the default) inherits the parent's config unchanged.
    pub fn with_ceiling(mut self, ceiling: ToolTrustTier) -> Self {
        self.trust_ceiling = Some(ceiling);
        self
    }

    /// Every finished child so far, in completion order.
    pub fn reports(&self) -> Vec<SubAgentSummary> {
        self.reports.lock().unwrap().clone()
    }

    /// The host's one-line swarm report (M8): the sub-agent count and
    /// chain ids, the swarm's tool-call total and token estimate (the
    /// parent's own counts included — the line states what the whole
    /// swarm burned) and the cost estimate with its method attached —
    /// the observatory, not the black box. `rate` is the parent's
    /// [`AgentConfig::cost_per_million_tokens`]; `None` drops the cost
    /// clause.
    pub fn summary_line(
        &self,
        parent_tool_calls: usize,
        parent_tokens: usize,
        rate: Option<f64>,
    ) -> String {
        let reports = self.reports();
        let ids: Vec<&str> = reports.iter().map(|r| r.task_id.as_str()).collect();
        let tool_calls: usize =
            reports.iter().map(|r| r.tool_calls).sum::<usize>() + parent_tool_calls;
        let tokens: usize =
            reports.iter().map(|r| r.tokens_estimated).sum::<usize>() + parent_tokens;
        let mut line = format!(
            "swarm: {} sub-agent(s) ({}), {} tool calls",
            reports.len(),
            ids.join(", "),
            tool_calls,
        );
        if let Some(rate) = rate {
            line.push_str(&format!(
                ", ~${:.2} in inference (estimate, chars/4, ${}/1M tokens)",
                tokens as f64 / 4.0 * rate / 1_000_000.0,
                rate,
            ));
        }
        line
    }

    /// A failed result for a spawn that never happened — no budget was
    /// consumed and no agent exists.
    fn refused(&self, call: &ToolCall, message: &str) -> ToolResult {
        ToolResult {
            tool_call_id: call.id.clone(),
            tool_name: SPAWN_AGENT.to_string(),
            success: false,
            output: json!({ "error": message }),
            display_summary: message.to_string(),
            duration_ms: 0,
        }
    }
}

#[async_trait]
impl ToolExecutor for SpawnAgentTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: SPAWN_AGENT.to_string(),
            description: "Delegate a sub-task to a child agent that runs the \
                 same loop under the same gate chain and trust ceiling — every \
                 tool call the child makes is judged exactly like yours, and the \
                 child's progress reports into the same event stream. The child \
                 sees only the given task prompt plus the system prompt, never \
                 your conversation. Returns the child's outcome: status, tool \
                 calls, token estimate and the head of its final answer."
                .to_string(),
            parameters: vec![ToolParam {
                name: "task".to_string(),
                description: "The sub-task prompt, self-contained: the child \
                     agent sees only this prompt and the same system prompt."
                    .to_string(),
                param_type: "string".to_string(),
                enum_values: None,
                required: true,
            }],
            trust_tier: ToolTrustTier::ExternalEffector,
        }
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        // The loop retries failed calls: replay a completed child run
        // instead of re-running it (see the `done` field).
        if let Some(result) = self.done.lock().unwrap().get(&call.id) {
            return result.clone();
        }

        let task = call.arg_str("task").unwrap_or("").trim().to_string();
        if task.is_empty() {
            return self.refused(call, "spawn_agent requires a non-empty `task` prompt");
        }

        // Budget first — fail closed before anything exists.
        {
            let mut remaining = self.budget.lock().unwrap();
            if *remaining == 0 {
                return self.refused(
                    call,
                    &format!(
                        "swarm budget exhausted: {} sub-agents max",
                        self.max_sub_agents
                    ),
                );
            }
            *remaining -= 1;
        }

        // The child's chain id: {parent}.{n}, per-parent ordinal.
        let n = {
            let mut counts = self.spawn_counts.lock().unwrap();
            let count = counts.entry(self.parent_task_id.clone()).or_insert(0);
            *count += 1;
            *count
        };
        let child_id = format!("{}.{n}", self.parent_task_id);
        self.parts.events.emit(&AgentEvent::SubAgentSpawned {
            task_id: child_id.clone(),
            parent_task_id: self.parent_task_id.clone(),
            prompt: task.clone(),
        });

        // The child's registry: the parent's tool set with spawn_agent
        // re-registered for the child's id. Any spawn instance the base
        // carried is replaced by name before the child ever runs.
        let child_tool = SpawnAgentTool {
            parts: self.parts.clone(),
            parent_task_id: child_id.clone(),
            budget: Arc::clone(&self.budget),
            spawn_counts: Arc::clone(&self.spawn_counts),
            max_sub_agents: self.max_sub_agents,
            trust_ceiling: self.trust_ceiling,
            reports: Arc::clone(&self.reports),
            done: Mutex::new(HashMap::new()),
        };
        let mut child_registry = self.parts.registry.clone();
        child_registry.register(Arc::new(child_tool));

        let mut child = Agent::new(
            Arc::clone(&self.parts.inference),
            child_registry,
            Arc::clone(&self.parts.policy),
        )
        .with_approval(Arc::clone(&self.parts.approval))
        .with_events(Arc::clone(&self.parts.events))
        .with_task_id(child_id.clone())
        .with_parent_task_id(self.parent_task_id.clone())
        .with_config(self.parts.config.clone());
        if let Some(privacy) = &self.parts.privacy {
            child = child.with_privacy(Arc::clone(privacy));
        }
        if let Some(path_policy) = &self.parts.path_policy {
            child = child.with_path_policy(Arc::clone(path_policy));
        }
        if let (Some(store), Some(tenant)) =
            (&self.parts.checkpoints, &self.parts.checkpoint_tenant)
        {
            child = child.with_checkpoints(Arc::clone(store), tenant.clone());
        }
        if let Some(ceiling) = self.trust_ceiling {
            child = child.with_config(AgentConfig {
                trust_ceiling: ceiling,
                ..self.parts.config.clone()
            });
        }

        let report = child.run(task.clone()).await;
        let error = report.steps.iter().rev().find_map(|step| match step {
            AgentStep::Error { message } => Some(message.clone()),
            _ => None,
        });
        let final_answer_head = report.final_answer.as_deref().map(truncate);
        let summary = SubAgentSummary {
            task_id: child_id.clone(),
            parent_task_id: self.parent_task_id.clone(),
            status: report.status,
            tokens_estimated: report.tokens_estimated,
            tool_calls: report.tool_calls,
            final_answer_head: final_answer_head.clone(),
        };
        self.reports.lock().unwrap().push(summary.clone());

        let display_summary = match (&summary.status, &final_answer_head) {
            (TaskStatus::Complete, Some(head)) => {
                format!("sub-agent {} completed: {}", child_id, head)
            }
            _ => format!(
                "sub-agent {} failed: {}",
                child_id,
                error
                    .as_deref()
                    .map(truncate)
                    .unwrap_or_else(|| "no final answer".to_string())
            ),
        };
        let mut output = json!({ "sub_agent": summary });
        if let Some(error) = &error {
            output["error"] = json!(error);
        }
        let result = ToolResult {
            tool_call_id: call.id.clone(),
            tool_name: SPAWN_AGENT.to_string(),
            // The mechanism worked; the child's own outcome is `status`
            // in the output. A failed child must not be re-run by the
            // loop's retry policy — the cache above guarantees it is not.
            success: true,
            output,
            display_summary,
            duration_ms: 0,
        };
        self.done
            .lock()
            .unwrap()
            .insert(call.id.clone(), result.clone());
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::{ApprovalGate, ApprovalRequest, AutoApprove};
    use crate::events::{AgentEvent, EventSink, FanoutSink, InMemoryEventSink};
    use crate::ledger_sink::LedgerSink;
    use crate::test_support::{
        registry_with, turn_text, turn_tool_call, AllowAllPolicy, EchoTool, ScriptedProvider,
    };
    use amparo_inference::InferenceError;
    use amparo_privacy::LedgerStore;
    use amparo_tools::registry::default_registry_with_policy;
    use amparo_tools::{PathPolicy, ToolRegistry, BLACKBOARD_READ, BLACKBOARD_WRITE};
    use async_trait::async_trait;
    use serde_json::Value;
    use std::sync::atomic::Ordering;

    /// The parts-donor parent (no spawn tool), the spawn tool, and the real
    /// parent (whose registry carries the tool). `events` is the shared
    /// sink both parent and children emit into.
    fn swarm(
        provider: Arc<ScriptedProvider>,
        base: &ToolRegistry,
        budget: Arc<Mutex<usize>>,
        max: usize,
        ceiling: Option<ToolTrustTier>,
    ) -> (Agent, Arc<SpawnAgentTool>, Arc<InMemoryEventSink>) {
        let events = Arc::new(InMemoryEventSink::new());
        let donor = Agent::new(provider.clone(), base.clone(), Arc::new(AllowAllPolicy))
            .with_approval(Arc::new(AutoApprove))
            .with_events(Arc::clone(&events) as Arc<dyn EventSink>)
            .with_task_id("sess-123");
        let mut tool = SpawnAgentTool::new(&donor, "sess-123", budget, max);
        if let Some(tier) = ceiling {
            tool = tool.with_ceiling(tier);
        }
        let tool = Arc::new(tool);
        let mut registry = base.clone();
        registry.register(Arc::clone(&tool) as Arc<dyn ToolExecutor>);
        let parent = Agent::new(provider, registry, Arc::new(AllowAllPolicy))
            .with_approval(Arc::new(AutoApprove))
            .with_events(Arc::clone(&events) as Arc<dyn EventSink>)
            .with_task_id("sess-123");
        (parent, tool, events)
    }

    fn spawn_call(id: &str, task: &str) -> Vec<String> {
        turn_tool_call(id, SPAWN_AGENT, &format!(r#"{{"task":"{task}"}}"#))
    }

    #[tokio::test]
    async fn spawn_runs_the_child_and_reports_its_summary() {
        let provider = ScriptedProvider::new();
        // One provider, one queue: the parent's spawn call, then the
        // child's answer (consumed mid-parent-loop), then the parent's.
        provider.push_chat(spawn_call("call_1", "research X"));
        provider.push_chat(turn_text("child found X"));
        provider.push_chat(turn_text("parent relays the child answer"));

        let (echo, _) = EchoTool::new(ToolTrustTier::Observational);
        let base = registry_with(Arc::new(echo));
        let budget = Arc::new(Mutex::new(2));
        let (parent, tool, events) = swarm(provider, &base, Arc::clone(&budget), 2, None);

        let report = parent.run("delegate research").await;
        assert_eq!(report.status, TaskStatus::Complete);
        assert_eq!(
            report.final_answer.as_deref(),
            Some("parent relays the child answer")
        );

        let reports = tool.reports();
        assert_eq!(reports.len(), 1);
        let child = &reports[0];
        assert_eq!(child.task_id, "sess-123.1");
        assert_eq!(child.parent_task_id, "sess-123");
        assert_eq!(child.status, TaskStatus::Complete);
        assert_eq!(child.final_answer_head.as_deref(), Some("child found X"));
        assert_eq!(*budget.lock().unwrap(), 1, "the spawn consumed one unit");

        // The shared sink carries the whole chain, child events interleaved
        // inside the parent's frame — the order a host renders in.
        let snapshot = events.snapshot();
        let position = |pred: &dyn Fn(&AgentEvent) -> bool| {
            snapshot
                .iter()
                .position(pred)
                .expect("event must be on the stream")
        };
        let parent_start = position(
            &|e| matches!(e, AgentEvent::TaskStarted { task_id, .. } if task_id.as_deref() == Some("sess-123")),
        );
        let spawn = position(
            &|e| matches!(e, AgentEvent::SubAgentSpawned { task_id, .. } if task_id == "sess-123.1"),
        );
        let child_start = position(
            &|e| matches!(e, AgentEvent::TaskStarted { task_id, .. } if task_id.as_deref() == Some("sess-123.1")),
        );
        let child_done = position(
            &|e| matches!(e, AgentEvent::TaskComplete { task_id, .. } if task_id.as_deref() == Some("sess-123.1")),
        );
        let parent_done = position(
            &|e| matches!(e, AgentEvent::TaskComplete { task_id, .. } if task_id.as_deref() == Some("sess-123")),
        );
        assert!(
            parent_start < spawn
                && spawn < child_start
                && child_start < child_done
                && child_done < parent_done
        );

        // The parent's own record carries the spawn result for the model.
        let spawn_result = report.steps.iter().find_map(|step| match step {
            AgentStep::ToolResult(result) if result.tool_name == SPAWN_AGENT => Some(result),
            _ => None,
        });
        let spawn_result = spawn_result.expect("the spawn call is a recorded step");
        assert!(spawn_result.success);
        assert_eq!(spawn_result.output["sub_agent"]["status"], "complete");
        assert_eq!(spawn_result.output["sub_agent"]["task_id"], "sess-123.1");
    }

    #[tokio::test]
    async fn budget_exhaustion_fails_closed_without_spawning() {
        let provider = ScriptedProvider::new();
        provider.push_chat(spawn_call("call_1", "first child"));
        provider.push_chat(turn_text("child one done"));
        provider.push_chat(spawn_call("call_2", "second child"));
        provider.push_chat(turn_text("parent wraps up"));

        let (echo, _) = EchoTool::new(ToolTrustTier::Observational);
        let base = registry_with(Arc::new(echo));
        let budget = Arc::new(Mutex::new(1));
        let (parent, tool, events) = swarm(provider, &base, Arc::clone(&budget), 1, None);

        let report = parent.run("run two children").await;
        assert_eq!(report.status, TaskStatus::Complete);
        // Only the first spawn ever ran — the second failed closed.
        let spawn_results: Vec<_> = report
            .steps
            .iter()
            .filter_map(|step| match step {
                AgentStep::ToolResult(result) if result.tool_name == SPAWN_AGENT => Some(result),
                _ => None,
            })
            .collect();
        assert_eq!(spawn_results.len(), 2, "one result per requested spawn");
        assert!(spawn_results[0].success);
        assert!(!spawn_results[1].success);
        let error = spawn_results[1].output["error"].as_str().unwrap();
        assert_eq!(error, "swarm budget exhausted: 1 sub-agents max");

        assert_eq!(tool.reports().len(), 1);
        let spawns = events
            .snapshot()
            .into_iter()
            .filter(|e| matches!(e, AgentEvent::SubAgentSpawned { .. }))
            .count();
        assert_eq!(
            spawns, 1,
            "an exhausted spawn emits nothing — no agent exists"
        );
        assert_eq!(*budget.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn blackboard_is_shared_across_the_spawn_boundary() {
        let provider = ScriptedProvider::new();
        // One provider, one queue: the parent's spawn, the child's board
        // write and its answer, the parent's board read and its answer.
        provider.push_chat(spawn_call("call_1", "leave the handoff"));
        provider.push_chat(turn_tool_call(
            "call_2",
            BLACKBOARD_WRITE,
            r#"{"key":"handoff","value":"from the child"}"#,
        ));
        provider.push_chat(turn_text("child wrote the handoff"));
        provider.push_chat(turn_tool_call(
            "call_3",
            BLACKBOARD_READ,
            r#"{"key":"handoff"}"#,
        ));
        provider.push_chat(turn_text("parent read the handoff"));

        let root = std::env::temp_dir().join(format!(
            "amparo-board-swarm-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let base = default_registry_with_policy(Arc::new(PathPolicy::from_root(root.clone())));
        let budget = Arc::new(Mutex::new(2));
        let (parent, _tool, events) = swarm(provider, &base, Arc::clone(&budget), 2, None);

        let report = parent.run("coordinate").await;
        assert_eq!(report.status, TaskStatus::Complete);
        assert_eq!(
            report.final_answer.as_deref(),
            Some("parent read the handoff")
        );

        // The board file under the workspace holds the child's row — the
        // child inherited the parent's registry, store and path.
        let rows = std::fs::read_to_string(root.join(".amparo/blackboard/board.jsonl"))
            .expect("the child's write created the board");
        assert!(rows.contains("from the child"), "{rows}");

        // The `[bus]` row names the child (the trusted writer, never the
        // caller), and the parent's read saw the child's value.
        let snapshot = events.snapshot();
        assert!(snapshot.iter().any(|e| matches!(
            e,
            AgentEvent::BlackboardWrite { key, written_by }
                if key == "handoff" && written_by.as_deref() == Some("sess-123.1")
        )));
        let read = snapshot
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolExecuted { result } if result.tool_name == BLACKBOARD_READ => {
                    Some(result)
                }
                _ => None,
            })
            .expect("the parent's board read executed");
        assert!(read.success);
        assert_eq!(read.output["value"], "from the child");
    }

    #[tokio::test]
    async fn grandchildren_share_the_one_budget() {
        let provider = ScriptedProvider::new();
        // Parent spawns; the child itself spawns a grandchild (gated by the
        // child's own chain); the grandchild answers, then child, then parent.
        provider.push_chat(spawn_call("call_1", "handle the details"));
        provider.push_chat(spawn_call("call_2", "sub-detail"));
        provider.push_chat(turn_text("grandchild done"));
        provider.push_chat(turn_text("child done"));
        provider.push_chat(turn_text("parent done"));

        let (echo, _) = EchoTool::new(ToolTrustTier::Observational);
        let base = registry_with(Arc::new(echo));
        let budget = Arc::new(Mutex::new(2));
        let (parent, tool, events) = swarm(provider, &base, Arc::clone(&budget), 2, None);

        let report = parent.run("delegate").await;
        assert_eq!(report.status, TaskStatus::Complete);

        let reports = tool.reports();
        assert_eq!(reports.len(), 2);
        // Completion order: the grandchild finishes first.
        assert_eq!(reports[0].task_id, "sess-123.1.1");
        assert_eq!(reports[0].parent_task_id, "sess-123.1");
        assert_eq!(reports[1].task_id, "sess-123.1");
        assert_eq!(reports[1].parent_task_id, "sess-123");
        assert_eq!(
            *budget.lock().unwrap(),
            0,
            "both spawns drew from one budget"
        );

        let ids: Vec<String> = events
            .snapshot()
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::SubAgentSpawned { task_id, .. } => Some(task_id),
                _ => None,
            })
            .collect();
        assert_eq!(
            ids,
            vec!["sess-123.1".to_string(), "sess-123.1.1".to_string()]
        );
    }

    #[tokio::test]
    async fn ceiling_override_blocks_the_childs_calls() {
        let provider = ScriptedProvider::new();
        provider.push_chat(spawn_call("call_1", "touch the outside world"));
        // The child tries a tool call above the overridden ceiling, then
        // answers anyway (a blocked call is an observation, not a stop).
        provider.push_chat(turn_tool_call("call_2", "echo", r#"{"message":"hi"}"#));
        provider.push_chat(turn_text("child answered without the tool"));
        provider.push_chat(turn_text("parent done"));

        let (echo, echo_calls) = EchoTool::new(ToolTrustTier::ExternalEffector);
        let base = registry_with(Arc::new(echo));
        let budget = Arc::new(Mutex::new(1));
        let (parent, _, events) = swarm(
            provider,
            &base,
            Arc::clone(&budget),
            1,
            Some(ToolTrustTier::Observational),
        );

        let report = parent.run("delegate").await;
        assert_eq!(report.status, TaskStatus::Complete);
        assert_eq!(
            echo_calls.load(Ordering::SeqCst),
            0,
            "the child's call never executed"
        );
        assert!(events.snapshot().iter().any(|e| matches!(
            e,
            AgentEvent::ToolGate { tool_name, decision, .. }
                if tool_name == "echo" && decision == "trust_blocked"
        )));
    }

    #[tokio::test]
    async fn missing_task_refuses_without_touching_the_budget() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_tool_call("call_1", SPAWN_AGENT, r#"{}"#));
        provider.push_chat(turn_text("parent moves on"));

        let (echo, _) = EchoTool::new(ToolTrustTier::Observational);
        let base = registry_with(Arc::new(echo));
        let budget = Arc::new(Mutex::new(1));
        let (parent, tool, events) = swarm(provider, &base, Arc::clone(&budget), 1, None);

        let report = parent.run("spawn badly").await;
        assert_eq!(report.status, TaskStatus::Complete);
        assert!(tool.reports().is_empty(), "no child ever ran");
        assert_eq!(*budget.lock().unwrap(), 1, "a refused spawn spends nothing");
        assert!(
            !events
                .snapshot()
                .iter()
                .any(|e| matches!(e, AgentEvent::SubAgentSpawned { .. })),
            "no agent exists, so no spawn event"
        );
        let spawn_result = report.steps.iter().find_map(|step| match step {
            AgentStep::ToolResult(result) if result.tool_name == SPAWN_AGENT => Some(result),
            _ => None,
        });
        let spawn_result = spawn_result.expect("the refused call is still a recorded step");
        assert!(!spawn_result.success);
        assert_eq!(
            spawn_result.output["error"].as_str().unwrap(),
            "spawn_agent requires a non-empty `task` prompt"
        );
    }

    #[tokio::test]
    async fn a_failed_child_is_not_rerun_by_the_retry_policy() {
        // The child fails (its first inference request errors); the loop's
        // retry policy must replay the cached result, not re-run the child —
        // observable here because a re-run would consume a second unit
        // of budget and emit a second spawn event.
        let provider = ScriptedProvider::new();
        provider.push_chat(spawn_call("call_1", "do the impossible"));
        provider.push_chat_error(InferenceError::Provider("child inference down".into()));
        provider.push_chat(turn_text("parent done"));

        let (echo, _) = EchoTool::new(ToolTrustTier::ExternalEffector);
        let base = registry_with(Arc::new(echo));
        let budget = Arc::new(Mutex::new(2));
        let (parent, tool, events) = swarm(provider, &base, Arc::clone(&budget), 2, None);

        let report = parent.run("delegate").await;
        assert_eq!(report.status, TaskStatus::Complete);
        assert_eq!(tool.reports().len(), 1);
        assert_eq!(
            *budget.lock().unwrap(),
            1,
            "the failed child ran exactly once"
        );
        let spawns = events
            .snapshot()
            .into_iter()
            .filter(|e| matches!(e, AgentEvent::SubAgentSpawned { .. }))
            .count();
        assert_eq!(spawns, 1);
        // The child's status reaches the parent as data, not as a retryable
        // tool failure: the spawn result itself succeeded.
        let spawn_result = report.steps.iter().find_map(|step| match step {
            AgentStep::ToolResult(result) if result.tool_name == SPAWN_AGENT => Some(result),
            _ => None,
        });
        let spawn_result = spawn_result.expect("the spawn call is a recorded step");
        assert!(spawn_result.success);
        assert_eq!(spawn_result.output["sub_agent"]["status"], "failed");
        assert!(
            spawn_result.output["error"]
                .as_str()
                .unwrap_or("")
                .contains("child inference down"),
            "the child's failure reason reaches the parent as data"
        );
    }

    #[tokio::test]
    async fn with_spawn_agent_registers_the_tool_on_the_parent() {
        // The host path (M8 W4): one builder call attaches the tool to
        // the agent's own registry and names the parent task.
        let provider = ScriptedProvider::new();
        provider.push_chat(spawn_call("call_1", "do the sub-task"));
        provider.push_chat(turn_text("child done"));
        provider.push_chat(turn_text("parent done"));

        let (echo, _) = EchoTool::new(ToolTrustTier::Observational);
        let events = Arc::new(InMemoryEventSink::new());
        let budget = Arc::new(Mutex::new(1));
        let (parent, tool) = Agent::new(
            provider,
            registry_with(Arc::new(echo)),
            Arc::new(AllowAllPolicy),
        )
        .with_approval(Arc::new(AutoApprove))
        .with_events(Arc::clone(&events) as Arc<dyn EventSink>)
        .with_spawn_agent("sess-9", Arc::clone(&budget), 1);

        let report = parent.run("delegate").await;
        assert_eq!(report.status, TaskStatus::Complete);
        let reports = tool.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].task_id, "sess-9.1");
        assert_eq!(*budget.lock().unwrap(), 0);
        // The summary line carries the chain id and, with a rate, the
        // cost estimate with its method attached — parent counts included.
        let line = tool.summary_line(report.tool_calls, report.tokens_estimated, None);
        assert!(
            line.starts_with("swarm: 1 sub-agent(s) (sess-9.1)"),
            "{line}"
        );
        let line = tool.summary_line(report.tool_calls, report.tokens_estimated, Some(3.0));
        assert!(line.contains("chars/4, $3/1M tokens"), "{line}");
    }

    /// The one-rule gate for the denial test (M8 W6): a top-level task's
    /// calls are approved, a sub-agent's are denied — so the child's
    /// gated call lands a denial row while the parent's spawn passes.
    struct ParentApprovesChildDenies;

    #[async_trait]
    impl ApprovalGate for ParentApprovesChildDenies {
        async fn request(&self, request: &ApprovalRequest) -> bool {
            request.session_label.is_none()
        }
    }

    #[tokio::test]
    async fn denied_child_call_lands_a_human_denied_row_with_the_chain() {
        // The cross-feature denial proof (M8 W6): the child's gated
        // command is refused at the shared gate, and the privacy ledger
        // records the denial stamped with the delegation chain — a
        // sub-agent's refused call is exactly as auditable as an
        // executed one.
        let dir = std::env::temp_dir().join(format!("amparo-spawn-denied-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ledger_path = dir.join("ledger.jsonl");

        let provider = ScriptedProvider::new();
        provider.push_chat(spawn_call("call_1", "run the child command"));
        provider.push_chat(turn_tool_call(
            "call_2",
            "run_command",
            r#"{"command":"echo child-work"}"#,
        ));
        provider.push_chat(turn_text("child done"));
        provider.push_chat(turn_text("parent done"));

        let base = default_registry_with_policy(Arc::new(PathPolicy::from_env()));
        let gate: Arc<dyn ApprovalGate> = Arc::new(ParentApprovesChildDenies);
        let events = Arc::new(InMemoryEventSink::new());
        let ledger = Arc::new(LedgerSink::new(
            LedgerStore::open(&ledger_path).unwrap(),
            "tenant",
            Some("sess-123".to_string()),
            None,
        ));
        let sink: Arc<dyn EventSink> = Arc::new(FanoutSink::new(vec![
            Arc::clone(&events) as Arc<dyn EventSink>,
            Arc::clone(&ledger) as Arc<dyn EventSink>,
        ]));
        let donor = Agent::new(provider.clone(), base.clone(), Arc::new(AllowAllPolicy))
            .with_approval(Arc::clone(&gate))
            .with_events(Arc::clone(&sink))
            .with_path_policy(Arc::new(PathPolicy::from_env()))
            .with_task_id("sess-123");
        let budget = Arc::new(Mutex::new(2));
        let tool = Arc::new(SpawnAgentTool::new(
            &donor,
            "sess-123",
            Arc::clone(&budget),
            2,
        ));
        let mut registry = base;
        registry.register(Arc::clone(&tool) as Arc<dyn ToolExecutor>);
        let parent = Agent::new(provider, registry, Arc::new(AllowAllPolicy))
            .with_approval(gate)
            .with_events(sink)
            .with_path_policy(Arc::new(PathPolicy::from_env()))
            .with_task_id("sess-123");

        let report = parent.run("delegate").await;
        assert_eq!(report.status, TaskStatus::Complete);
        assert_eq!(tool.reports().len(), 1, "the spawn ran");

        // The denied child call is the ledger row — the loop never
        // executes a denied call, but the audit question has its answer,
        // stamped with the chain the CLI e2e proves for executions.
        let text = std::fs::read_to_string(&ledger_path).expect("ledger exists");
        let rows: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).expect("one ledger row per line"))
            .collect();
        assert_eq!(rows.len(), 1, "one denial row: {text}");
        assert_eq!(rows[0]["tool"], "run_command");
        assert_eq!(rows[0]["outcome"], "denied");
        assert_eq!(rows[0]["gate"], "human_denied");
        assert_eq!(rows[0]["task_id"], "sess-123.1");
        assert_eq!(rows[0]["parent_task_id"], "sess-123");
        assert!(
            !text.contains("child-work"),
            "the command never reaches the ledger: {text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
