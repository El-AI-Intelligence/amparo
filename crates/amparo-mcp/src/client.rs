//! The Amparo MCP client — mount external MCP tools into the registry.
//!
//! [`McpClient::spawn`] starts an MCP server process, performs the
//! `initialize` handshake, and fetches its tool list. [`McpClient::mount_into`]
//! registers each remote tool as a [`ToolExecutor`] in a
//! [`amparo_tools::ToolRegistry`] — which means an Amparo agent sees them as
//! ordinary registry tools and runs them through the **same policy gate**
//! as every other call. Remote tools default to the `ExternalEffector`
//! trust tier (they execute outside this process), which routes them to the
//! approval gate unless the operator deliberately mounts them lower.

use crate::types::{
    first_text, mcp_tool_params, CallToolResult, ClientCapabilities, ClientInfo,
    InitializeRequest, McpTool, PROTOCOL_VERSION,
};
use amparo_tools::{ToolCall, ToolExecutor, ToolParam, ToolRegistry, ToolResult, ToolTrustTier};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{oneshot, Mutex};

#[derive(Debug, Error)]
pub enum McpError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON-RPC error {code}: {message}")]
    JsonRpc { code: i64, message: String },
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("MCP server exited before the session completed")]
    ProcessExit,
    #[error("call timed out after {0:?}")]
    Timeout(Duration),
    #[error("session closed")]
    Closed,
}

/// How long a single `tools/call` may take before the client gives up.
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(60);

type PendingSender = oneshot::Sender<Result<Value, McpError>>;

/// The live connection to one MCP server: a reader task routing responses
/// by id, and a mutex-guarded writer.
struct McpSession {
    writer: Mutex<Box<dyn AsyncWrite + Unpin + Send>>,
    pending: Mutex<HashMap<Value, PendingSender>>,
    next_id: AtomicU64,
    call_timeout: Duration,
}

impl McpSession {
    async fn send_request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = Value::from(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);
        let line = serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        })
        .to_string();
        {
            let mut writer = self.writer.lock().await;
            writer.write_all(line.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
        }
        match tokio::time::timeout(self.call_timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(McpError::Closed),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(McpError::Timeout(self.call_timeout))
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let line = serde_json::json!({
            "jsonrpc": "2.0", "method": method, "params": params
        })
        .to_string();
        let mut writer = self.writer.lock().await;
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        Ok(())
    }

    /// Spawn the reader task: one response line at a time, routed to the
    /// awaiting caller by id. When the stream ends (server exit), every
    /// pending call fails with `Closed` — a dead server is never a silent
    /// hang.
    fn spawn_reader<R>(self: &Arc<Self>, reader: R)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        let session = Arc::clone(self);
        tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue; // stray non-JSON on stdout — skip, don't die
                };
                let Some(id) = value.get("id").cloned() else {
                    continue; // server→client notifications are ignored
                };
                let outcome = if let Some(err) = value.get("error") {
                    Err(McpError::JsonRpc {
                        code: err.get("code").and_then(|c| c.as_i64()).unwrap_or(-32603),
                        message: err
                            .get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("unknown error")
                            .to_string(),
                    })
                } else {
                    Ok(value.get("result").cloned().unwrap_or(Value::Null))
                };
                if let Some(tx) = session.pending.lock().await.remove(&id) {
                    let _ = tx.send(outcome);
                }
            }
            // EOF: the server is gone — fail everything still waiting.
            let pending: Vec<PendingSender> =
                session.pending.lock().await.drain().map(|(_, tx)| tx).collect();
            for tx in pending {
                let _ = tx.send(Err(McpError::Closed));
            }
        });
    }
}

/// A connected MCP server, ready to handshake and mount.
pub struct McpClient {
    session: Arc<McpSession>,
    /// Owned only so the child process dies when the client does.
    _child: Option<tokio::process::Child>,
    tools: Vec<McpTool>,
}

impl McpClient {
    /// Spawn an MCP server process and complete the handshake
    /// (`initialize` → `notifications/initialized` → `tools/list`).
    pub async fn spawn(
        program: impl AsRef<OsStr>,
        args: impl IntoIterator<Item = impl AsRef<OsStr>>,
    ) -> Result<Self, McpError> {
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| McpError::Protocol("no stdin".into()))?;
        let stdout =
            child.stdout.take().ok_or_else(|| McpError::Protocol("no stdout".into()))?;
        let client = Self::from_io(stdout, stdin, DEFAULT_CALL_TIMEOUT).await?;
        Ok(Self { _child: Some(child), ..client })
    }

    /// Build a session over arbitrary streams — used by tests (duplex
    /// pipes) and by hosts that already own a connected process.
    pub async fn from_io<R, W>(reader: R, writer: W, call_timeout: Duration) -> Result<Self, McpError>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let session = Arc::new(McpSession {
            writer: Mutex::new(Box::new(writer)),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            call_timeout,
        });
        session.spawn_reader(reader);

        let init: Value = session
            .send_request(
                "initialize",
                serde_json::to_value(InitializeRequest {
                    protocolVersion: PROTOCOL_VERSION.to_string(),
                    capabilities: ClientCapabilities { tools: Some(serde_json::json!({})) },
                    clientInfo: ClientInfo {
                        name: "amparo".to_string(),
                        version: env!("CARGO_PKG_VERSION").to_string(),
                    },
                })
                .map_err(|e| McpError::Protocol(e.to_string()))?,
            )
            .await?;
        if init.get("protocolVersion").and_then(|v| v.as_str()).is_none() {
            return Err(McpError::Protocol("server answered initialize without protocolVersion".into()));
        }
        session.notify("notifications/initialized", serde_json::json!({})).await?;

        let listed: Value = session.send_request("tools/list", serde_json::json!({})).await?;
        let tools: Vec<McpTool> = serde_json::from_value(
            listed.get("tools").cloned().ok_or_else(|| {
                McpError::Protocol("tools/list answered without a tools array".into())
            })?,
        )
        .map_err(|e| McpError::Protocol(format!("tools/list tools parse failed: {e}")))?;

        Ok(Self { session, _child: None, tools })
    }

    /// The remote tools discovered at handshake time.
    pub fn tools(&self) -> &[McpTool] {
        &self.tools
    }

    /// Register every remote tool as a registry executor at the given trust
    /// tier. Callers choosing anything below `ExternalEffector` accept that
    /// those remote calls then execute without human approval; the default
    /// posture is to mount at `ExternalEffector` so the approval gate covers
    /// them.
    pub async fn mount_into(&self, registry: &mut ToolRegistry, tier: ToolTrustTier) {
        for tool in &self.tools {
            let executor = RemoteMcpTool {
                session: Arc::clone(&self.session),
                tool: tool.clone(),
                tier,
            };
            registry.register(Arc::new(executor));
        }
    }

    /// Liveness check against the remote server.
    pub async fn ping(&self) -> Result<(), McpError> {
        self.session.send_request("ping", serde_json::json!({})).await.map(|_| ())
    }

    /// Call a remote tool directly (not through a registry).
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<CallToolResult, McpError> {
        let result: Value = self
            .session
            .send_request(
                "tools/call",
                serde_json::json!({"name": name, "arguments": arguments}),
            )
            .await?;
        serde_json::from_value(result).map_err(|e| McpError::Protocol(e.to_string()))
    }
}

/// One remote tool, mounted as a registry executor. Executing it sends a
/// `tools/call` to the remote server and shapes the answer into a
/// [`ToolResult`] the agent loop already understands.
pub struct RemoteMcpTool {
    session: Arc<McpSession>,
    tool: McpTool,
    tier: ToolTrustTier,
}

impl RemoteMcpTool {
    fn parameters(&self) -> Vec<ToolParam> {
        mcp_tool_params(&self.tool)
    }
}

#[async_trait]
impl ToolExecutor for RemoteMcpTool {
    fn schema(&self) -> amparo_tools::ToolSchema {
        amparo_tools::ToolSchema {
            name: self.tool.name.clone(),
            description: if self.tool.description.is_empty() {
                format!("Remote MCP tool ({}), mounted from an external MCP server", self.tool.name)
            } else {
                self.tool.description.clone()
            },
            parameters: self.parameters(),
            trust_tier: self.tier,
        }
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        let result = self
            .session
            .send_request(
                "tools/call",
                serde_json::json!({"name": call.name, "arguments": call.arguments}),
            )
            .await
            .and_then(|value| {
                serde_json::from_value::<CallToolResult>(value)
                    .map_err(|e| McpError::Protocol(e.to_string()))
            });
        match result {
            Ok(mcp) => ToolResult {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                success: !mcp.isError,
                output: serde_json::to_value(&mcp.content).unwrap_or(serde_json::json!([])),
                display_summary: truncate(&first_text(&mcp), 200),
                duration_ms: 0,
            },
            Err(e) => ToolResult {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                success: false,
                output: serde_json::json!({"error": e.to_string()}),
                display_summary: format!("MCP error: {e}"),
                duration_ms: 0,
            },
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::McpServer;
    use amparo_policy::{DenyAllPolicyEngine, PolicyDecision};
    use std::sync::atomic::{AtomicUsize, Ordering};

    // In-process e2e: the real server speaks to the real client over a
    // duplex pipe — the full initialize → tools/list → tools/call path
    // without a subprocess in the way.

    #[tokio::test]
    async fn client_handshakes_and_calls_through_duplex_pipe() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool { calls: calls.clone() }));

        struct AllowAll;
        #[async_trait]
        impl amparo_policy::PolicyEngine for AllowAll {
            async fn judge_tool(
                &self,
                _t: &str,
                _x: &str,
                _p: &[(&str, &str)],
            ) -> PolicyDecision {
                PolicyDecision::allow()
            }
        }

        let server = McpServer::new(registry, Arc::new(AllowAll))
            .with_approval(Arc::new(amparo_agent::AutoApprove));

        let (client_reader, server_writer) = tokio::io::duplex(64 * 1024);
        let (server_reader, client_writer) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            server.serve(server_reader, server_writer).await.unwrap();
        });

        let client = McpClient::from_io(client_reader, client_writer, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(client.tools().len(), 1);
        assert_eq!(client.tools()[0].name, "echo");
        client.ping().await.unwrap();

        let mut mounted = ToolRegistry::new();
        client.mount_into(&mut mounted, ToolTrustTier::ExternalEffector).await;
        let result = mounted
            .dispatch(&ToolCall {
                id: "t1".into(),
                name: "echo".into(),
                arguments: serde_json::json!({"message": "through the pipe"}),
            })
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the real tool ran behind the protocol");
    }

    #[tokio::test]
    async fn server_policy_denial_surfaces_as_failed_tool_result() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool { calls: Arc::new(AtomicUsize::new(0)) }));
        let server = McpServer::new(registry, Arc::new(DenyAllPolicyEngine::new("test")));

        let (client_reader, server_writer) = tokio::io::duplex(64 * 1024);
        let (server_reader, client_writer) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            server.serve(server_reader, server_writer).await.unwrap();
        });

        let client = McpClient::from_io(client_reader, client_writer, Duration::from_secs(5))
            .await
            .unwrap();
        let call = ToolCall {
            id: "t2".into(),
            name: "echo".into(),
            arguments: serde_json::json!({"message": "nope"}),
        };
        let mut mounted = ToolRegistry::new();
        client.mount_into(&mut mounted, ToolTrustTier::ExternalEffector).await;
        let result = mounted.dispatch(&call).await.unwrap();
        assert!(!result.success);
        assert!(result.output.to_string().contains("Policy denied echo"));
    }

    // A minimal tool for the pipe tests.
    struct EchoTool {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ToolExecutor for EchoTool {
        fn schema(&self) -> amparo_tools::ToolSchema {
            amparo_tools::ToolSchema {
                name: "echo".to_string(),
                description: "echo".to_string(),
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
                display_summary: format!("echoed: {}", call.arg_str("message").unwrap_or("")),
                duration_ms: 0,
            }
        }
    }
}
