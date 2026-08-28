//! Amparo Tools — the registry and the portable tool set.
//!
//! Every tool has a JSON schema (exposed to the LLM), a trust tier (drives the
//! approval gate), and a sandboxed execution path. The registry itself holds
//! no policy — the deny-by-default policy gate lives in `amparo-agent`.

pub mod build;
pub mod filesystem;
pub mod git;
pub mod memory;
pub mod paths;
pub mod registry;
pub mod shell;
pub mod testing;
pub mod web;

pub use memory::{InMemoryStore, Memory, MemoryEntry, MemorySearchTool, MemoryWriteTool};
pub use paths::PathPolicy;
pub use registry::{
    default_registry, ToolCall, ToolExecutor, ToolParam, ToolRegistry, ToolResult, ToolSchema,
    ToolTrustTier,
};
