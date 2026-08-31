// Originally part of Axiom-OS (MIT, Copyright (c) Pixel Phantom AI) —
// `tools/mod.rs`, the tool system core.
// Ported to Amparo and relicensed Apache-2.0 — see the repository NOTICE.
//
// Amparo's registry drops the desktop-bound tools (screenshot, display,
// automation, framegraph, mesh), the notification surface, and the stub
// leaves (email, wallet, inbox). What remains is the portable, deployable
// set: web, filesystem, shell, git, tests, build, memory.

//! Tool registry — the contracts and the central registry of Amparo's
//! portable tool set.
//!
//! Defines the JSON-schema parameters exposed to the LLM, the trust tiers
//! that drive the approval gate, the call/result types, the [`ToolExecutor`]
//! trait, and the [`ToolRegistry`] that holds every registered tool. The
//! registry holds no policy — the deny-by-default approval gate lives in
//! `amparo-agent`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

// ─────────────────────────────────────────────── Trust / safety tier ─────────

/// Trust tier for a tool — drives the approval gate in the agent loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolTrustTier {
    /// Read-only, no side effects (web search, memory read)
    Observational = 0,
    /// Write to local memory/workspace, no external side effects
    LocalMutating = 1,
    /// External side effects — requires human approval before execution
    ExternalEffector = 2,
    /// System-level (run command, deploy) — requires human approval
    SystemControl = 3,
}

// ─────────────────────────────────────────────────── Tool definition ─────────

/// JSON-schema parameter definition exposed to the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolParam {
    /// Parameter name as exposed to the LLM.
    pub name: String,
    /// Human-readable description of the parameter.
    pub description: String,
    /// JSON-schema type for the parameter (e.g. "string", "integer",
    /// "boolean").
    #[serde(rename = "type")]
    pub param_type: String,
    /// Allowed values, when the parameter is a closed enum.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<String>>,
    /// Whether the LLM must supply this parameter on every call.
    #[serde(default)]
    pub required: bool,
}

/// Full tool schema exposed to the LLM via the inference request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    /// Tool name as exposed to the LLM.
    pub name: String,
    /// Human-readable description of what the tool does.
    pub description: String,
    /// JSON-schema parameter definitions exposed to the LLM.
    pub parameters: Vec<ToolParam>,
    /// Trust tier for this tool, which drives the approval gate in
    /// `amparo-agent`.
    pub trust_tier: ToolTrustTier,
}

impl ToolSchema {
    /// Render as OpenAI-compatible function definition.
    pub fn to_openai_function(&self) -> Value {
        let props: serde_json::Map<String, Value> = self
            .parameters
            .iter()
            .map(|p| {
                let mut obj = serde_json::Map::new();
                obj.insert("type".to_string(), Value::String(p.param_type.clone()));
                obj.insert(
                    "description".to_string(),
                    Value::String(p.description.clone()),
                );
                if let Some(enums) = &p.enum_values {
                    obj.insert(
                        "enum".to_string(),
                        Value::Array(enums.iter().map(|e| Value::String(e.clone())).collect()),
                    );
                }
                (p.name.clone(), Value::Object(obj))
            })
            .collect();

        let required: Vec<Value> = self
            .parameters
            .iter()
            .filter(|p| p.required)
            .map(|p| Value::String(p.name.clone()))
            .collect();

        serde_json::json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": {
                    "type": "object",
                    "properties": props,
                    "required": required,
                }
            }
        })
    }
}

// ─────────────────────────────────────────────────── Tool call / result ───────

/// A tool call emitted by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Unique id for this call, echoed back in the matching [`ToolResult`].
    pub id: String,
    /// Name of the tool being called.
    pub name: String,
    /// JSON arguments to the tool.
    pub arguments: Value,
}

impl ToolCall {
    /// Get a string argument.
    pub fn arg_str(&self, key: &str) -> Option<&str> {
        self.arguments.get(key).and_then(|v| v.as_str())
    }

    /// Get a u64 argument, coercing string values (LLMs often pass numbers as strings).
    pub fn arg_u64(&self, key: &str) -> Option<u64> {
        self.arguments.get(key).and_then(|v| {
            if let Some(n) = v.as_u64() {
                return Some(n);
            }
            v.as_str().and_then(|s| s.parse().ok())
        })
    }

    /// Get a bool argument, coercing string values ("true"/"false").
    pub fn arg_bool(&self, key: &str) -> Option<bool> {
        self.arguments.get(key).and_then(|v| {
            if let Some(b) = v.as_bool() {
                return Some(b);
            }
            v.as_str().and_then(|s| s.parse().ok())
        })
    }
}

/// Result returned after executing a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// Id of the originating [`ToolCall`].
    pub tool_call_id: String,
    /// Name of the tool that produced this result.
    pub tool_name: String,
    /// Whether the tool reported success.
    pub success: bool,
    /// JSON payload with the tool's output data.
    pub output: Value,
    /// Human-readable summary for display in chat
    pub display_summary: String,
    /// Wall-clock execution time, in milliseconds.
    pub duration_ms: u64,
}

/// A tool-declared rollback hint (M10 W3): how a human can undo a call
/// after it ran.
///
/// **Display-only by design (I1).** Nothing executes a rollback
/// automatically — that would be auto-policy. The spec rides on the
/// approval copy and the `[rollback]` event row so a human always
/// knows the undo path. `undo` must describe an *idempotent* action
/// (running it twice changes nothing further); `markers` name the
/// file-backup artifacts the call preserved for that undo, when any.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackSpec {
    /// The idempotent undo action, in plain language.
    pub undo: String,
    /// File-backup markers the call created (for example the
    /// `.amparo-bak` copy of the pre-call state), empty when the undo
    /// needs none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub markers: Vec<String>,
}

// ─────────────────────────────────────────────────────── ToolExecutor trait ──

/// Every tool implements this trait.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Returns the tool's schema, including its name, parameters, and trust
    /// tier.
    fn schema(&self) -> ToolSchema;
    /// Executes a tool call and returns its result.
    async fn execute(&self, call: &ToolCall) -> ToolResult;

    /// The rollback hint for `call` (M10 W3), or `None` when the call
    /// has no meaningful idempotent undo.
    ///
    /// Computed from the call's arguments against the *pre-call*
    /// state — callers must invoke this before execution so the undo
    /// describes exactly what the call is about to change. The default
    /// is `None`; destructive-class tools (file writes and deletes)
    /// override it.
    fn rollback(&self, _call: &ToolCall) -> Option<RollbackSpec> {
        None
    }
}

// ─────────────────────────────────────────────── ToolRegistry ────────────────

/// Central registry — holds all available tools, dispatches calls, exposes schemas.
///
/// Cheap to clone (the executors are `Arc`s behind one map), which is what
/// lets hosts build one registry and hand a copy to each per-task agent.
#[derive(Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn ToolExecutor>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Registers a tool executor under its schema name, replacing any tool
    /// with the same name.
    pub fn register(&mut self, executor: Arc<dyn ToolExecutor>) {
        let name = executor.schema().name.clone();
        self.tools.insert(name, executor);
    }

    /// Schemas for all registered tools, rendered as OpenAI function definitions.
    pub fn openai_tools(&self) -> Vec<Value> {
        self.tools
            .values()
            .map(|t| t.schema().to_openai_function())
            .collect()
    }

    /// Schemas for tools at or below a given trust tier.
    pub fn openai_tools_for_tier(&self, max_tier: ToolTrustTier) -> Vec<Value> {
        self.tools
            .values()
            .filter(|t| t.schema().trust_tier <= max_tier)
            .map(|t| t.schema().to_openai_function())
            .collect()
    }

    /// Dispatches a call to the named tool and returns its result, or `None`
    /// when no such tool is registered.
    pub async fn dispatch(&self, call: &ToolCall) -> Option<ToolResult> {
        if let Some(executor) = self.tools.get(&call.name) {
            let start = std::time::Instant::now();
            let mut result = executor.execute(call).await;
            result.duration_ms = start.elapsed().as_millis() as u64;
            Some(result)
        } else {
            None
        }
    }

    /// Returns the schemas of all registered tools.
    pub fn list_schemas(&self) -> Vec<ToolSchema> {
        self.tools.values().map(|t| t.schema()).collect()
    }

    /// Returns the trust tier for a tool by name.
    pub fn get_tier(&self, name: &str) -> Option<ToolTrustTier> {
        self.tools.get(name).map(|t| t.schema().trust_tier)
    }

    /// Returns a cloned Arc to the executor so callers can invoke it after
    /// releasing the registry lock (avoids holding a Mutex across an .await).
    pub fn get_executor(&self, name: &str) -> Option<Arc<dyn ToolExecutor>> {
        self.tools.get(name).map(Arc::clone)
    }

    /// The rollback hint the registered tool declares for `call`
    /// (M10 W3), or `None` when no tool is registered or the tool
    /// declares none. See [`ToolExecutor::rollback`] — compute this
    /// before execution, while the pre-call state still holds.
    pub fn rollback_for(&self, call: &ToolCall) -> Option<RollbackSpec> {
        self.tools.get(&call.name).and_then(|t| t.rollback(call))
    }
}

// ─────────────────────────────────────────── Build default registry ──────────

use crate::blackboard::{BlackboardReadTool, BlackboardStore, BlackboardWriteTool};
use crate::build::RunBuildTool;
use crate::filesystem::{EditFileTool, ListDirTool, PatchFileTool, ReadFileTool, WriteFileTool};
use crate::git::{
    GitBlameTool, GitBranchTool, GitCommitTool, GitDiffTool, GitLogTool, GitStatusTool,
};
use crate::memory::{Memory, MemorySearchTool};
use crate::notification::SendNotificationTool;
use crate::paths::PathPolicy;
use crate::shell::RunCommandTool;
use crate::testing::RunTestsTool;
use crate::web::{FetchUrlTool, WebSearchTool};

/// Create the default registry with all 20 built-in tools, the 14
/// workspace-bound ones rooted at `policy` — plus the blackboard, rooted
/// at the policy's workspace root, and `send_notification` on its stderr
/// default transport (hosts re-register it with their own seam).
///
/// The deny-by-default posture lives in the *agent*, not here: a registry
/// registers tools; only the policy gate in `amparo-agent` decides whether a
/// call executes.
pub fn default_registry_with_policy(policy: Arc<PathPolicy>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(WebSearchTool::new()));
    registry.register(Arc::new(FetchUrlTool::new()));
    registry.register(Arc::new(ReadFileTool::with_policy(Arc::clone(&policy))));
    registry.register(Arc::new(WriteFileTool::with_policy(Arc::clone(&policy))));
    registry.register(Arc::new(ListDirTool::with_policy(Arc::clone(&policy))));
    registry.register(Arc::new(EditFileTool::with_policy(Arc::clone(&policy))));
    registry.register(Arc::new(PatchFileTool::with_policy(Arc::clone(&policy))));
    registry.register(Arc::new(RunCommandTool::with_policy(Arc::clone(&policy))));
    registry.register(Arc::new(MemorySearchTool::new()));
    // The blackboard (M10): one store shared by both tools and, through
    // the registry clone handed to sub-agents, by the whole delegation
    // chain. Rooted at the workspace — the same policy every other
    // workspace-bound tool is confined by.
    let board = Arc::new(BlackboardStore::new(&policy.workspace_root));
    registry.register(Arc::new(BlackboardReadTool::new(Arc::clone(&board))));
    registry.register(Arc::new(BlackboardWriteTool::new(board)));
    // The notification tool (M10 W2) ships on the stderr default; hosts
    // replace it by re-registering with their own transport.
    registry.register(Arc::new(SendNotificationTool::to_stderr()));
    registry.register(Arc::new(GitStatusTool::with_policy(Arc::clone(&policy))));
    registry.register(Arc::new(GitDiffTool::with_policy(Arc::clone(&policy))));
    registry.register(Arc::new(GitCommitTool::with_policy(Arc::clone(&policy))));
    registry.register(Arc::new(GitLogTool::with_policy(Arc::clone(&policy))));
    registry.register(Arc::new(GitBranchTool::with_policy(Arc::clone(&policy))));
    registry.register(Arc::new(GitBlameTool::with_policy(Arc::clone(&policy))));
    registry.register(Arc::new(RunTestsTool::with_policy(Arc::clone(&policy))));
    registry.register(Arc::new(RunBuildTool::with_policy(Arc::clone(&policy))));
    registry
}

/// Create the default registry with all built-in Amparo tools registered.
///
/// Equivalent to [`default_registry_with_policy`] with the policy loaded from
/// the environment.
pub fn default_registry() -> ToolRegistry {
    default_registry_with_policy(Arc::new(PathPolicy::from_env()))
}

/// Create the default registry with the given memory backend behind
/// `memory_search` (M11 W1): `memory` replaces the built-in in-memory
/// store the default registers. Equivalent to
/// [`default_registry_with_policy_and_memory`] with the policy loaded
/// from the environment.
pub fn default_registry_with_memory(memory: Arc<dyn Memory>) -> ToolRegistry {
    default_registry_with_policy_and_memory(Arc::new(PathPolicy::from_env()), memory)
}

/// Create the default registry rooted at `policy`, with the given memory
/// backend behind `memory_search` (M11 W1): `memory` replaces the
/// built-in in-memory store [`default_registry_with_policy`] registers —
/// re-registering the search tool is how hosts wire e.g. the Engram
/// adapter without touching the default.
pub fn default_registry_with_policy_and_memory(
    policy: Arc<PathPolicy>,
    memory: Arc<dyn Memory>,
) -> ToolRegistry {
    let mut registry = default_registry_with_policy(policy);
    registry.register(Arc::new(MemorySearchTool::with_store(memory)));
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_function_shape_is_valid() {
        let schema = ToolSchema {
            name: "read_file".to_string(),
            description: "read".to_string(),
            parameters: vec![ToolParam {
                name: "path".to_string(),
                description: "file path".to_string(),
                param_type: "string".to_string(),
                enum_values: None,
                required: true,
            }],
            trust_tier: ToolTrustTier::Observational,
        };
        let v = schema.to_openai_function();
        assert_eq!(v["type"], "function");
        assert_eq!(v["function"]["name"], "read_file");
        assert_eq!(v["function"]["parameters"]["required"][0], "path");
        assert_eq!(
            v["function"]["parameters"]["properties"]["path"]["type"],
            "string"
        );
    }

    #[test]
    fn enum_values_render_as_enum() {
        let schema = ToolSchema {
            name: "write_file".to_string(),
            description: "write".to_string(),
            parameters: vec![ToolParam {
                name: "mode".to_string(),
                description: "mode".to_string(),
                param_type: "string".to_string(),
                enum_values: Some(vec!["overwrite".into(), "append".into()]),
                required: false,
            }],
            trust_tier: ToolTrustTier::LocalMutating,
        };
        let v = schema.to_openai_function();
        let e = &v["function"]["parameters"]["properties"]["mode"]["enum"];
        assert_eq!(e[0], "overwrite");
        assert_eq!(e[1], "append");
    }

    #[test]
    fn tier_filter_excludes_higher_tiers() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(ReadFileTool::new())); // Observational
        reg.register(Arc::new(RunCommandTool::new())); // ExternalEffector
        let names: Vec<String> = reg
            .openai_tools_for_tier(ToolTrustTier::LocalMutating)
            .iter()
            .map(|v| v["function"]["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"read_file".to_string()));
        assert!(!names.contains(&"run_command".to_string()));
    }

    #[test]
    fn arg_coercion_accepts_string_numbers_and_bools() {
        let call = ToolCall {
            id: "1".to_string(),
            name: "x".to_string(),
            arguments: serde_json::json!({"n": "7", "b": "true"}),
        };
        assert_eq!(call.arg_u64("n"), Some(7));
        assert_eq!(call.arg_bool("b"), Some(true));
    }

    #[test]
    fn default_registry_has_no_desktop_or_stub_tools() {
        let reg = default_registry();
        let names: Vec<String> = reg.list_schemas().iter().map(|s| s.name.clone()).collect();
        for banned in [
            "take_screenshot",
            "click",
            "type_text",
            "send_email",
            "wallet_send",
            "check_inbox",
        ] {
            assert!(
                !names.contains(&banned.to_string()),
                "{} must not be in the Amparo registry",
                banned
            );
        }
        for required in [
            "run_command",
            "read_file",
            "write_file",
            "git_commit",
            "web_search",
            "run_tests",
        ] {
            assert!(
                names.contains(&required.to_string()),
                "{} must be in the registry",
                required
            );
        }
    }

    #[tokio::test]
    async fn default_registry_with_policy_roots_workspace_tools() {
        let root =
            std::env::temp_dir().join(format!("amparo-registry-policy-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("seed.txt"), "injected content").unwrap();
        let reg = default_registry_with_policy(Arc::new(PathPolicy::from_root(root.clone())));

        let read = reg
            .dispatch(&ToolCall {
                id: "1".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "seed.txt"}),
            })
            .await
            .expect("read_file must be registered");
        assert!(read.success, "output: {}", read.output);
        assert_eq!(read.output["content"], "injected content");

        let pwd = reg
            .dispatch(&ToolCall {
                id: "2".to_string(),
                name: "run_command".to_string(),
                arguments: serde_json::json!({"command": "pwd"}),
            })
            .await
            .expect("run_command must be registered");
        assert!(pwd.success, "output: {}", pwd.output);
        let stdout = pwd.output["stdout"].as_str().unwrap_or("");
        assert!(
            stdout.contains(&root.to_string_lossy().to_string()),
            "pwd must report the injected root, got: {}",
            stdout
        );
    }
}
