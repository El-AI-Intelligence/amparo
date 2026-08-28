//! Amparo MCP — Model Context Protocol, both directions, one policy gate.
//!
//! - [`server::McpServer`] exposes a [`amparo_tools::ToolRegistry`] to
//!   external MCP clients over stdio JSON-RPC 2.0. Exposing tools does not
//!   weaken the gate: every `tools/call` runs registry lookup → trust
//!   ceiling → policy (deny-by-default, the engine is a constructor
//!   argument) → human approval (auto-deny unless wired).
//! - [`client::McpClient`] spawns an external MCP server, handshakes
//!   (`initialize` → `notifications/initialized` → `tools/list`), and mounts
//!   its tools into an Amparo registry as ordinary executors — so an Amparo
//!   agent drives them through the same gate chain as every other tool.
//!   Remote tools mount at `ExternalEffector` by default (they execute
//!   outside this process), which routes them to the approval gate unless
//!   the operator deliberately mounts them lower.
//!
//! The stdio transport is newline-delimited JSON-RPC 2.0, stdout carries
//! protocol lines only, and a dead server fails every pending call rather
//! than hanging the agent.

pub mod client;
pub mod jsonrpc;
pub mod serve;
pub mod server;
pub mod types;

pub use client::{McpClient, McpError, RemoteMcpTool};
pub use server::McpServer;
pub use types::{CallToolResult, McpTool, PROTOCOL_VERSION};
