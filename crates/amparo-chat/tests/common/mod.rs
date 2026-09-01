// Shared test doubles for amparo-chat: a scripted inference provider and a
// recording chat transport.
//
// Unit tests reach this file from each `src/` test module with
// `mod common { include!("../tests/common/mod.rs"); }` — `#[path]` cannot
// reach it (its base includes the virtual module path, which no real
// directory matches), while `include!` resolves relative to the actual
// source file. The integration tests (Telegram, Discord, Slack) include it
// the usual way (`mod common;`). No network is ever touched: StubProvider
// streams scripted frames, and MockTransport records every outbound call.
// The unit-test consumers wrap this file in `mod common { include!(...) }`
// and mark that module `#[allow(dead_code)]`; integration-test consumers do
// the same with their `mod common;`. An inner `#![allow]` here is rejected
// by the include! context, so it lives at the consumers instead.

use amparo_agent::ApprovalRequest;
use amparo_chat::driver::ChatDriver;
use amparo_chat::transport::{ApprovalMessage, ChatError, ChatRef, ChatTransport};
use amparo_inference::{
    ChatRequest, InferenceError, InferenceProvider, InferenceRequest, InferenceResponse,
    InferenceStream,
};
use amparo_tools::{
    ToolCall, ToolExecutor, ToolParam, ToolRegistry, ToolResult, ToolSchema, ToolTrustTier,
};
use async_trait::async_trait;
use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long [`wait_until`] polls before failing the test.
///
/// 5s proved too tight for loaded CI runners — a turn that runs four
/// fetch_url calls plus ledger writes can exceed it purely on scheduler
/// contention, especially on 2-core Windows hosts. The budget is a
/// polling ceiling, not a correctness window: a stalled driver still
/// fails, just at 30s instead of a premature 5s.
const WAIT_BUDGET: Duration = Duration::from_secs(30);

/// Poll `cond` every 10ms until it holds, or fail the test after
/// [`WAIT_BUDGET`].
pub async fn wait_until(cond: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT_BUDGET;
    loop {
        if cond() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "condition not met within {WAIT_BUDGET:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Async sibling of [`wait_until`] for conditions that must await — e.g.
/// polling a store whose writes land on a spawned task.
pub async fn wait_until_async<Fut>(cond: impl Fn() -> Fut)
where
    Fut: Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + WAIT_BUDGET;
    loop {
        if cond().await {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "condition not met within {WAIT_BUDGET:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Wait until the transport has recorded a text containing `needle`, then
/// return it.
pub async fn wait_for_text(transport: &Arc<MockTransport>, needle: &str) -> String {
    wait_until(|| transport.texts().iter().any(|t| t.contains(needle))).await;
    transport
        .texts()
        .into_iter()
        .find(|t| t.contains(needle))
        .expect("needle appeared between poll and read")
}

/// A recording [`ChatTransport`]: every outbound call is logged for
/// assertions, and `receive` never runs. [`fail_next_send`](Self::fail_next_send)
/// makes the next `send_approval` fail, for the gate's fail-closed test.
pub struct MockTransport {
    texts: Mutex<Vec<String>>,
    /// The [`ChatRef`] each text went to, in order — the route matters for
    /// the `send_notification` adapter (M10 W2).
    chats: Mutex<Vec<ChatRef>>,
    approvals: Mutex<Vec<ApprovalMessage>>,
    edits: Mutex<Vec<(ApprovalMessage, String)>>,
    /// Set to `true` to make the next `send_approval` return an error.
    pub fail_next_send: AtomicBool,
    /// Set to `true` to make the next `send_text` return an error.
    pub fail_next_text: AtomicBool,
}

impl MockTransport {
    /// An empty transport, ready to record.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            texts: Mutex::new(Vec::new()),
            chats: Mutex::new(Vec::new()),
            approvals: Mutex::new(Vec::new()),
            edits: Mutex::new(Vec::new()),
            fail_next_send: AtomicBool::new(false),
            fail_next_text: AtomicBool::new(false),
        })
    }

    /// Every text sent so far, in order.
    pub fn texts(&self) -> Vec<String> {
        self.texts.lock().unwrap().clone()
    }

    /// The most recently sent text's [`ChatRef`], if any — which chat the
    /// outbound line went to.
    pub fn last_chat(&self) -> Option<ChatRef> {
        self.chats.lock().unwrap().last().cloned()
    }

    /// Every approval message sent so far, in order.
    pub fn approvals(&self) -> Vec<ApprovalMessage> {
        self.approvals.lock().unwrap().clone()
    }

    /// Every approval edit as `(message, outcome)` pairs, in order.
    pub fn edits(&self) -> Vec<(ApprovalMessage, String)> {
        self.edits.lock().unwrap().clone()
    }
}

#[async_trait]
impl ChatTransport for MockTransport {
    async fn send_text(&self, chat: &ChatRef, text: &str) -> Result<(), ChatError> {
        if self.fail_next_text.swap(false, Ordering::SeqCst) {
            return Err(ChatError::Telegram("simulated text failure".into()));
        }
        self.texts.lock().unwrap().push(text.to_string());
        self.chats.lock().unwrap().push(chat.clone());
        Ok(())
    }

    async fn send_approval(
        &self,
        chat: &ChatRef,
        _request: &ApprovalRequest,
        _approval_id: &str,
    ) -> Result<ApprovalMessage, ChatError> {
        if self.fail_next_send.swap(false, Ordering::SeqCst) {
            return Err(ChatError::Telegram("simulated send failure".into()));
        }
        let message_id = format!("msg_{}", self.approvals.lock().unwrap().len());
        let msg = ApprovalMessage {
            chat_id: chat.chat_id.clone(),
            message_id,
        };
        self.approvals.lock().unwrap().push(msg.clone());
        Ok(msg)
    }

    async fn edit_approval(&self, msg: &ApprovalMessage, outcome: &str) -> Result<(), ChatError> {
        self.edits
            .lock()
            .unwrap()
            .push((msg.clone(), outcome.to_string()));
        Ok(())
    }

    async fn receive(self: Arc<Self>, _driver: Arc<ChatDriver>) -> Result<(), ChatError> {
        Ok(())
    }
}

/// A scripted [`InferenceProvider`]: each `complete_chat_stream` call pops
/// one SSE script (a `Vec<String>` of frames) and streams it, so the full
/// driver path runs without any HTTP. Exhausted scripts stream a plain
/// "Done." turn, and `complete` (the self-verification call) always answers
/// "VERIFIED" while recording the prompt so tests can observe what evidence
/// (if any) the verification carried.
pub struct StubProvider {
    scripts: Mutex<VecDeque<Vec<String>>>,
    panicking: AtomicBool,
    requests: Mutex<Vec<String>>,
    complete_prompts: Mutex<Vec<String>>,
}

impl StubProvider {
    /// A provider streaming the given scripts, one per chat turn.
    pub fn new(script: Vec<Vec<String>>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(script.into()),
            panicking: AtomicBool::new(false),
            requests: Mutex::new(Vec::new()),
            complete_prompts: Mutex::new(Vec::new()),
        })
    }

    /// A provider that panics on its first chat call — the driver's
    /// panic-containment test.
    pub fn panicking() -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(VecDeque::new()),
            panicking: AtomicBool::new(true),
            requests: Mutex::new(Vec::new()),
            complete_prompts: Mutex::new(Vec::new()),
        })
    }

    /// Every chat request answered so far, serialized — lets tests observe
    /// what the LLM was told (tool results, workspace paths).
    pub fn recorded_requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    /// Every `complete` (self-verification) prompt answered so far, in order.
    pub fn recorded_complete_prompts(&self) -> Vec<String> {
        self.complete_prompts.lock().unwrap().clone()
    }
}

/// A content-delta SSE frame.
pub fn content_frame(text: &str) -> String {
    serde_json::json!({"choices": [{"delta": {"content": text}}]}).to_string()
}

/// A tool-call-delta SSE frame.
pub fn tool_call_frame(id: &str, name: &str, arguments: &str) -> String {
    serde_json::json!({"choices": [{"delta": {"tool_calls": [{
        "index": 0, "id": id, "type": "function",
        "function": {"name": name, "arguments": arguments}
    }]}}]})
    .to_string()
}

/// The `[DONE]` frame that ends every script.
pub fn done_frame() -> String {
    "[DONE]".to_string()
}

/// A one-turn script: plain text, then done.
pub fn turn_text(text: &str) -> Vec<String> {
    vec![content_frame(text), done_frame()]
}

/// A one-turn script: a single tool call, then done.
pub fn turn_tool_call(id: &str, name: &str, arguments: &str) -> Vec<String> {
    vec![tool_call_frame(id, name, arguments), done_frame()]
}

fn sse_stream(frames: &[String]) -> InferenceStream {
    use futures_util::stream;
    let events: Vec<std::result::Result<bytes::Bytes, InferenceError>> = frames
        .iter()
        .map(|f| Ok(bytes::Bytes::from(format!("data: {f}\n\n"))))
        .collect();
    Box::pin(stream::iter(events))
}

#[async_trait]
impl InferenceProvider for StubProvider {
    async fn complete(
        &self,
        request: InferenceRequest,
    ) -> Result<InferenceResponse, InferenceError> {
        self.complete_prompts.lock().unwrap().push(request.prompt);
        Ok(InferenceResponse {
            text: "VERIFIED".into(),
            tokens: 1,
            finish_reason: "stop".into(),
        })
    }

    async fn complete_chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<InferenceStream, InferenceError> {
        if self.panicking.load(Ordering::SeqCst) {
            panic!("stub provider panicked in complete_chat_stream");
        }
        self.requests
            .lock()
            .unwrap()
            .push(serde_json::to_string(&request).unwrap_or_default());
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| turn_text("Done."));
        Ok(sse_stream(&script))
    }

    async fn embed(&self, _text: &str) -> Result<Vec<f64>, InferenceError> {
        Ok(vec![])
    }

    async fn list_models(&self) -> Result<Vec<String>, InferenceError> {
        Ok(vec![])
    }

    fn default_model(&self) -> String {
        "stub-model".into()
    }
}

/// A stub tool that echoes its `message` argument back as a successful
/// result — for agent-loop tests that need a real executor.
pub struct EchoTool {
    tier: ToolTrustTier,
}

impl EchoTool {
    /// An echo tool at the given trust tier (`ExternalEffector` forces the
    /// approval gate).
    pub fn new(tier: ToolTrustTier) -> Self {
        Self { tier }
    }
}

#[async_trait]
impl ToolExecutor for EchoTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "echo".to_string(),
            description: "echo the message".to_string(),
            parameters: vec![ToolParam {
                name: "message".to_string(),
                description: "text to echo".to_string(),
                param_type: "string".to_string(),
                enum_values: None,
                required: true,
            }],
            trust_tier: self.tier,
        }
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        ToolResult {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            success: true,
            output: serde_json::json!({"echoed": call.arg_str("message").unwrap_or("")}),
            display_summary: format!("echoed: {}", call.arg_str("message").unwrap_or("")),
            duration_ms: 0,
        }
    }
}

/// A registry holding a single echo tool at the given tier.
pub fn registry_with_echo(tier: ToolTrustTier) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(EchoTool::new(tier)));
    registry
}
