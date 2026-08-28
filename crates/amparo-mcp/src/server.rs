//! The Amparo MCP server — amparo tools over Model Context Protocol.
//!
//! A [`McpServer`] wraps a [`ToolRegistry`] and speaks JSON-RPC 2.0 over
//! stdio (newline-delimited, stdout only — stderr stays free for logs).
//!
//! Exposing tools over MCP does **not** bypass the policy gate. Every
//! `tools/call` runs the same per-call gate chain the agent loop uses:
//!
//! ```text
//! registry lookup → trust ceiling → policy gate → human approval → execute
//! ```
//!
//! The policy engine is a constructor argument (deny-by-default — there is
//! no engine-less server), and the approval gate defaults to auto-deny, so
//! a server nobody wired a human into refuses every escalated call.

use crate::jsonrpc;
use crate::types::{
    to_mcp_tool, CallToolResult, ClientInfo, ContentBlock, InitializeResult, ListToolsResult,
    ServerCapabilities, ServerToolsCapabilities, PROTOCOL_VERSION,
};
use amparo_agent::{ApprovalGate, ApprovalRequest, AutoDeny};
use amparo_policy::{PolicyEngine, PolicyVerdict};
use amparo_tools::{ToolCall, ToolRegistry, ToolResult, ToolTrustTier};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

pub struct McpServer {
    registry: ToolRegistry,
    policy: Arc<dyn PolicyEngine>,
    approval: Arc<dyn ApprovalGate>,
    trust_ceiling: ToolTrustTier,
    name: String,
    version: String,
    next_call_id: AtomicU64,
}

impl McpServer {
    /// The policy engine is required — a server with no engine refuses
    /// everything (pass [`amparo_policy::DenyAllPolicyEngine`] explicitly).
    pub fn new(registry: ToolRegistry, policy: Arc<dyn PolicyEngine>) -> Self {
        Self {
            registry,
            policy,
            approval: Arc::new(AutoDeny),
            trust_ceiling: ToolTrustTier::SystemControl,
            name: "amparo".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            next_call_id: AtomicU64::new(1),
        }
    }

    /// Swap the policy engine after construction (useful in tests and for
    /// hosts that decide the engine late).
    pub fn with_policy(mut self, policy: Arc<dyn PolicyEngine>) -> Self {
        self.policy = policy;
        self
    }

    /// Wire a human-approval gate (default: auto-deny).
    pub fn with_approval(mut self, gate: Arc<dyn ApprovalGate>) -> Self {
        self.approval = gate;
        self
    }

    /// Tools above this tier are refused outright (default: SystemControl).
    pub fn with_trust_ceiling(mut self, ceiling: ToolTrustTier) -> Self {
        self.trust_ceiling = ceiling;
        self
    }

    /// The identity advertised in `initialize` responses.
    pub fn with_info(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        self.name = name.into();
        self.version = version.into();
        self
    }

    /// Handle one inbound line. Requests get a response line; notifications
    /// and blank lines get `None`; unparseable input gets a parse-error
    /// response with a null id.
    pub async fn handle_line(&self, line: &str) -> Option<String> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let msg = match jsonrpc::parse(line) {
            Ok(m) => m,
            Err(e) => {
                return Some(jsonrpc::error(&Value::Null, &e));
            }
        };
        // Notifications are never answered.
        let id = match &msg.id {
            Some(id) => id,
            None => return None,
        };
        Some(match msg.method.as_str() {
            "initialize" => self.handle_initialize(id, &msg.params),
            "tools/list" => self.handle_list(id),
            "tools/call" => self.handle_call(id, &msg.params).await,
            "ping" => jsonrpc::success(id, serde_json::json!({})),
            _ => jsonrpc::error(id, &jsonrpc::RpcError::method_not_found()),
        })
    }

    /// Read newline-delimited requests from `reader`, writing responses to
    /// `writer`. Never writes anything but protocol lines to `writer`.
    pub async fn serve<R, W>(&self, reader: R, writer: W) -> std::io::Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut lines = BufReader::new(reader).lines();
        let mut writer = writer;
        while let Some(line) = lines.next_line().await? {
            if let Some(response) = self.handle_line(&line).await {
                writer.write_all(response.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                writer.flush().await?;
            }
        }
        Ok(())
    }

    /// Serve this process's stdio — the canonical embedding.
    pub async fn serve_stdio(&self) -> std::io::Result<()> {
        let stdin = tokio::io::BufReader::new(tokio::io::stdin());
        let stdout = tokio::io::stdout();
        self.serve(stdin, stdout).await
    }

    fn handle_initialize(&self, id: &Value, params: &Option<Value>) -> String {
        // The client may request any protocol version; the server answers
        // with the one it speaks and the client decides whether to proceed.
        let result = InitializeResult {
            protocolVersion: PROTOCOL_VERSION.to_string(),
            capabilities: ServerCapabilities {
                tools: ServerToolsCapabilities { listChanged: false },
            },
            serverInfo: ClientInfo { name: self.name.clone(), version: self.version.clone() },
            instructions: Some(
                "Amparo tools execute behind a policy gate. Denied and escalated \
                 calls return isError results with the reasons."
                    .to_string(),
            ),
        };
        let _ = params;
        jsonrpc::success(id, serde_json::to_value(result).unwrap_or(Value::Null))
    }

    fn handle_list(&self, id: &Value) -> String {
        let tools: Vec<_> = self.registry.list_schemas().iter().map(to_mcp_tool).collect();
        let result =
            serde_json::to_value(ListToolsResult { tools }).unwrap_or(serde_json::json!({}));
        jsonrpc::success(id, result)
    }

    async fn handle_call(&self, id: &Value, params: &Option<Value>) -> String {
        let Some(params) = params else {
            return jsonrpc::error(
                id,
                &jsonrpc::RpcError::invalid_params("tools/call requires params"),
            );
        };
        let Some(name) = params.get("name").and_then(|v| v.as_str()) else {
            return jsonrpc::error(
                id,
                &jsonrpc::RpcError::invalid_params("tools/call requires a string name"),
            );
        };
        let arguments =
            params.get("arguments").cloned().unwrap_or(serde_json::json!({}));
        let call = ToolCall {
            id: format!("mcp_{}", self.next_call_id.fetch_add(1, Ordering::Relaxed)),
            name: name.to_string(),
            arguments,
        };
        let result = self.gate_and_dispatch(&call).await;
        let text = serde_json::to_string_pretty(&result.output).unwrap_or_default();
        let mcp_result = CallToolResult {
            content: vec![ContentBlock { block_type: "text".to_string(), text }],
            isError: !result.success,
        };
        jsonrpc::success(id, serde_json::to_value(mcp_result).unwrap_or(Value::Null))
    }

    /// The gate chain — the same order and semantics as the agent loop's
    /// per-call path: registry lookup, trust ceiling, policy (deny by
    /// default), human approval for escalated or external-effector calls,
    /// then execution.
    async fn gate_and_dispatch(&self, call: &ToolCall) -> ToolResult {
        let fail = |call: &ToolCall, error: String| ToolResult {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            success: false,
            output: serde_json::json!({"error": error}),
            display_summary: "Blocked".to_string(),
            duration_ms: 0,
        };

        if self.registry.get_executor(&call.name).is_none() {
            let available: Vec<String> =
                self.registry.list_schemas().iter().map(|s| s.name.clone()).collect();
            return fail(
                call,
                format!("Unknown tool: {}. Available: {}", call.name, available.join(", ")),
            );
        }

        if let Some(tier) = self.registry.get_tier(&call.name) {
            if tier > self.trust_ceiling {
                return fail(call, "tool blocked by trust ceiling".to_string());
            }
        }

        let (target, params) = amparo_agent::extract_target(call);
        let param_refs: Vec<(&str, &str)> =
            params.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let decision = self.policy.judge_tool(&call.name, &target, &param_refs).await;
        match decision.verdict {
            PolicyVerdict::Deny => {
                return fail(
                    call,
                    format!("Policy denied {}: {}", call.name, decision.fired.join("; ")),
                );
            }
            PolicyVerdict::Escalate => {
                let tier = self
                    .registry
                    .get_tier(&call.name)
                    .unwrap_or(ToolTrustTier::Observational);
                let mut reasons = decision.fired;
                reasons.push("policy escalated this call for human review".to_string());
                let request = ApprovalRequest {
                    call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    arguments: call.arguments.clone(),
                    reasons,
                };
                if !self.approval.request(&request).await {
                    return fail(call, "User denied the action or approval timed out".to_string());
                }
                let _ = tier;
            }
            PolicyVerdict::Allow => {}
        }

        let tier = self
            .registry
            .get_tier(&call.name)
            .unwrap_or(ToolTrustTier::Observational);
        if tier >= ToolTrustTier::ExternalEffector {
            let request = ApprovalRequest {
                call_id: call.id.clone(),
                tool_name: call.name.clone(),
                arguments: call.arguments.clone(),
                reasons: vec![format!("tool tier {:?} requires human approval", tier)],
            };
            if !self.approval.request(&request).await {
                return fail(call, "User denied the action or approval timed out".to_string());
            }
        }

        self.registry
            .dispatch(call)
            .await
            .unwrap_or_else(|| fail(call, "tool dispatch failed".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{McpTool, PROTOCOL_VERSION};
    use amparo_agent::{ApprovalGate, AutoApprove};
    use amparo_policy::{DenyAllPolicyEngine, PolicyDecision};
    use amparo_tools::{ToolExecutor, ToolParam, ToolSchema};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct EchoTool {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ToolExecutor for EchoTool {
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "echo".to_string(),
                description: "echo a message".to_string(),
                parameters: vec![ToolParam {
                    name: "message".to_string(),
                    description: "text".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: true,
                }],
                trust_tier: ToolTrustTier::Observational,
            }
        }

        async fn execute(&self, call: &ToolCall) -> ToolResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            ToolResult {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                success: true,
                output: serde_json::json!({"echoed": call.arg_str("message")}),
                display_summary: "echoed".to_string(),
                duration_ms: 0,
            }
        }
    }

    fn echo_server() -> (McpServer, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool { calls: calls.clone() }));
        (McpServer::new(registry, Arc::new(DenyAllPolicyEngine::new("test"))), calls)
    }

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

    struct RecordingGate {
        requests: Mutex<Vec<ApprovalRequest>>,
        answer: bool,
    }

    #[async_trait]
    impl ApprovalGate for RecordingGate {
        async fn request(&self, request: &ApprovalRequest) -> bool {
            self.requests.lock().unwrap().push(request.clone());
            self.answer
        }
    }

    fn call_line(id: u64, name: &str, arguments: Value) -> String {
        serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        })
        .to_string()
    }

    #[tokio::test]
    async fn initialize_advertises_tools_capability() {
        let (server, _) = echo_server();
        let out = server
            .handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(v["result"]["serverInfo"]["name"], "amparo");
        assert_eq!(v["result"]["capabilities"]["tools"]["listChanged"], false);
    }

    #[tokio::test]
    async fn tools_list_renders_registry_schemas() {
        let (server, _) = echo_server();
        let out = server
            .handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let tools: Vec<McpTool> =
            serde_json::from_value(v["result"]["tools"].clone()).unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].inputSchema.required, vec!["message"]);
    }

    #[tokio::test]
    async fn policy_deny_blocks_tools_call_with_reasons() {
        let (server, calls) = echo_server();
        let out = server
            .handle_line(&call_line(3, "echo", serde_json::json!({"message": "hi"})))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["result"]["isError"], true);
        assert!(v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Policy denied echo"));
        assert_eq!(calls.load(Ordering::SeqCst), 0, "denied tools never execute");
    }

    #[tokio::test]
    async fn allowed_call_executes_and_returns_content() {
        let (server, calls) = echo_server();
        let server = server.with_policy(Arc::new(AllowAll)).with_approval(Arc::new(AutoApprove));
        let out = server
            .handle_line(&call_line(4, "echo", serde_json::json!({"message": "hi"})))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["result"]["isError"], false);
        assert!(v["result"]["content"][0]["text"].as_str().unwrap().contains("hi"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unknown_tool_lists_available_tools() {
        let (server, _) = echo_server();
        let out = server
            .handle_line(&call_line(5, "nope", serde_json::json!({})))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Unknown tool: nope"));
        assert!(text.contains("echo"));
    }

    #[tokio::test]
    async fn unknown_method_gets_method_not_found() {
        let (server, _) = echo_server();
        let out = server
            .handle_line(r#"{"jsonrpc":"2.0","id":9,"method":"resources/list"}"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn notifications_are_never_answered() {
        let (server, _) = echo_server();
        let out = server
            .handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .await;
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn blank_and_garbage_lines() {
        let (server, _) = echo_server();
        assert!(server.handle_line("").await.is_none());
        assert!(server.handle_line("   ").await.is_none());
        let out = server.handle_line("garbage").await.unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error"]["code"], -32700);
    }

    struct EscalateAll;
    #[async_trait]
    impl PolicyEngine for EscalateAll {
        async fn judge_tool(
            &self,
            _t: &str,
            _x: &str,
            _p: &[(&str, &str)],
        ) -> PolicyDecision {
            PolicyDecision::escalate("test escalate")
        }
    }

    fn escalate_server(answer: bool) -> (McpServer, Arc<RecordingGate>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool { calls: calls.clone() }));
        let gate =
            Arc::new(RecordingGate { requests: Mutex::new(Vec::new()), answer });
        let server = McpServer::new(registry, Arc::new(EscalateAll)).with_approval(gate.clone());
        (server, gate, calls)
    }

    #[tokio::test]
    async fn escalated_call_asks_the_gate_and_respects_denial() {
        let (server, gate, calls) = escalate_server(false);
        let out = server
            .handle_line(&call_line(6, "echo", serde_json::json!({"message": "x"})))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["result"]["isError"], true);
        assert!(v["result"]["content"][0]["text"].as_str().unwrap().contains("denied"));
        assert_eq!(gate.requests.lock().unwrap().len(), 1);
        assert!(gate.requests.lock().unwrap()[0]
            .reasons
            .iter()
            .any(|r| r.contains("test escalate")));
        assert_eq!(calls.load(Ordering::SeqCst), 0, "denied escalated call never executes");
    }

    #[tokio::test]
    async fn escalated_call_executes_when_approved() {
        let (server, gate, calls) = escalate_server(true);
        let out = server
            .handle_line(&call_line(7, "echo", serde_json::json!({"message": "x"})))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["result"]["isError"], false);
        assert_eq!(gate.requests.lock().unwrap().len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
