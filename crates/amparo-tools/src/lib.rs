//! Amparo Tools — the registry and the portable tool set.
//!
//! Every tool has a JSON schema (exposed to the LLM), a trust tier (drives the
//! approval gate), and a sandboxed execution path. The registry itself holds
//! no policy — the deny-by-default policy gate lives in `amparo-agent`.

#![warn(missing_docs)]

pub mod blackboard;
pub mod build;
pub mod engram_store;
pub mod filesystem;
pub mod git;
pub mod memory;
pub mod notification;
pub mod org_policy;
pub mod paths;
pub mod process_env;
pub mod registry;
pub mod shell;
pub mod skills;
pub mod testing;
pub mod web;

pub use blackboard::{
    BlackboardEntry, BlackboardReadTool, BlackboardStore, BlackboardWriteTool, BLACKBOARD_DIR,
    BLACKBOARD_FILE, BLACKBOARD_READ, BLACKBOARD_WRITE,
};
pub use engram_store::{resolve_memory_backend, EngramStore, DEFAULT_ENGRAM_URL};
pub use memory::{InMemoryStore, Memory, MemoryEntry, MemorySearchTool, MemoryWriteTool};
pub use notification::{
    Notification, NotificationTransport, SendNotificationTool, StderrTransport, WebhookTransport,
    SEND_NOTIFICATION,
};
pub use org_policy::{
    OrgInfo, OrgPolicyClient, OrgPolicyError, OrgPolicyRule, DEFAULT_CONSOLE_POLICY_URL,
};
pub use paths::PathPolicy;
pub use registry::{
    default_registry, default_registry_with_memory, default_registry_with_policy_and_memory,
    RollbackSpec, ToolCall, ToolExecutor, ToolParam, ToolRegistry, ToolResult, ToolSchema,
    ToolTrustTier,
};
pub use skills::{SkillLibrary, SkillOrigin, SkillSpec, SkillStep, UseSkillTool, USE_SKILL};
