//! Test doubles shared across the crate's unit tests.
//!
//! The M7 pattern: a [`ScriptedProvider`] plays the model (one SSE script —
//! or one error — per chat turn, one completion reply per self-verification),
//! [`EchoTool`] forces
//! specific gate paths through its tier, [`RecordingGate`] answers approvals
//! with a fixed verdict while recording every request, and [`AllowAllPolicy`]
//! keeps the policy gate open. `spawn.rs` tests reuse the same doubles, so a
//! sub-agent run is scripted exactly like a top-level one.

use crate::approval::{ApprovalGate, ApprovalRequest};
use amparo_inference::{ChatRequest, InferenceError, InferenceProvider, InferenceRequest};
use amparo_policy::{PolicyDecision, PolicyEngine};
use amparo_tools::{
    ToolCall, ToolExecutor, ToolParam, ToolRegistry, ToolResult, ToolSchema, ToolTrustTier,
};
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// One scripted chat reply: either a turn's SSE frames, or an inference
/// error that fails the turn.
pub(crate) enum ChatReply {
    /// Play this script (a Vec of frames) as the turn's stream.
    Script(Vec<String>),
    /// Fail the turn with this error.
    Error(InferenceError),
}

/// Scripted provider: pops one [`ChatReply`] per chat turn from a single
/// FIFO queue — the call order of `push_chat`/`push_chat_error` is exactly
/// the order the agent (and any sub-agents sharing this provider) makes
/// requests; an empty queue plays a default "Done." turn. `complete()`
/// (used by self-verification) pops from a separate queue. Every chat
/// request and every completion request is recorded for assertions.
pub(crate) struct ScriptedProvider {
    chat_replies: std::sync::Mutex<std::collections::VecDeque<ChatReply>>,
    complete_scripts: std::sync::Mutex<std::collections::VecDeque<String>>,
    chat_requests: std::sync::Mutex<Vec<ChatRequest>>,
    complete_requests: std::sync::Mutex<Vec<InferenceRequest>>,
}

impl ScriptedProvider {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            chat_replies: std::sync::Mutex::new(Default::default()),
            complete_scripts: std::sync::Mutex::new(Default::default()),
            chat_requests: std::sync::Mutex::new(Vec::new()),
            complete_requests: std::sync::Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn push_chat(&self, script: Vec<String>) {
        self.chat_replies
            .lock()
            .unwrap()
            .push_back(ChatReply::Script(script));
    }

    pub(crate) fn push_chat_error(&self, err: InferenceError) {
        self.chat_replies
            .lock()
            .unwrap()
            .push_back(ChatReply::Error(err));
    }

    pub(crate) fn push_verify(&self, reply: &str) {
        self.complete_scripts
            .lock()
            .unwrap()
            .push_back(reply.to_string());
    }

    pub(crate) fn recorded_requests(&self) -> Vec<ChatRequest> {
        self.chat_requests.lock().unwrap().clone()
    }

    pub(crate) fn recorded_complete_requests(&self) -> Vec<InferenceRequest> {
        self.complete_requests.lock().unwrap().clone()
    }
}

pub(crate) fn content_delta(text: &str) -> String {
    serde_json::json!({"choices": [{"delta": {"content": text}}]}).to_string()
}

pub(crate) fn tool_call_frame(id: &str, name: &str, arguments: &str) -> String {
    serde_json::json!({"choices": [{"delta": {"tool_calls": [{
        "index": 0, "id": id, "type": "function",
        "function": {"name": name, "arguments": arguments}
    }]}}]})
    .to_string()
}

pub(crate) fn done() -> String {
    "[DONE]".to_string()
}

/// One full turn: a single tool call, then [DONE].
pub(crate) fn turn_tool_call(id: &str, name: &str, arguments: &str) -> Vec<String> {
    vec![tool_call_frame(id, name, arguments), done()]
}

/// One full turn: plain text, then [DONE].
pub(crate) fn turn_text(text: &str) -> Vec<String> {
    vec![content_delta(text), done()]
}

pub(crate) fn sse_stream(frames: &[String]) -> amparo_inference::InferenceStream {
    use futures_util::stream;
    let events: Vec<std::result::Result<bytes::Bytes, InferenceError>> = frames
        .iter()
        .map(|f| Ok(bytes::Bytes::from(format!("data: {}\n\n", f))))
        .collect();
    Box::pin(stream::iter(events))
}

#[async_trait]
impl InferenceProvider for ScriptedProvider {
    async fn complete(
        &self,
        request: InferenceRequest,
    ) -> Result<amparo_inference::InferenceResponse, InferenceError> {
        self.complete_requests.lock().unwrap().push(request);
        let reply = self
            .complete_scripts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| "VERIFIED".to_string());
        Ok(amparo_inference::InferenceResponse {
            text: reply,
            tokens: 1,
            finish_reason: "stop".into(),
        })
    }

    async fn complete_chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<amparo_inference::InferenceStream, InferenceError> {
        self.chat_requests.lock().unwrap().push(request);
        match self.chat_replies.lock().unwrap().pop_front() {
            Some(ChatReply::Script(script)) => Ok(sse_stream(&script)),
            Some(ChatReply::Error(err)) => Err(err),
            None => Ok(sse_stream(&turn_text("Done."))),
        }
    }

    async fn embed(&self, _text: &str) -> Result<Vec<f64>, InferenceError> {
        Ok(vec![])
    }

    async fn list_models(&self) -> Result<Vec<String>, InferenceError> {
        Ok(vec![])
    }

    fn default_model(&self) -> String {
        "test-model".into()
    }
}

/// A stub tool that counts its executions and echoes `message`.
pub(crate) struct EchoTool {
    calls: Arc<AtomicUsize>,
    tier: ToolTrustTier,
}

impl EchoTool {
    pub(crate) fn new(tier: ToolTrustTier) -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                calls: calls.clone(),
                tier,
            },
            calls,
        )
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
        self.calls.fetch_add(1, Ordering::SeqCst);
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

pub(crate) fn registry_with(executor: Arc<dyn ToolExecutor>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(executor);
    registry
}

/// Allow everything, record nothing.
pub(crate) struct AllowAllPolicy;

#[async_trait]
impl PolicyEngine for AllowAllPolicy {
    async fn judge_tool(
        &self,
        _tool: &str,
        _target: &str,
        _params: &[(&str, &str)],
    ) -> PolicyDecision {
        PolicyDecision::allow()
    }
}

/// Records approval requests and answers with a fixed verdict.
pub(crate) struct RecordingGate {
    requests: std::sync::Mutex<Vec<ApprovalRequest>>,
    answer: bool,
}

impl RecordingGate {
    pub(crate) fn new(answer: bool) -> Arc<Self> {
        Arc::new(Self {
            requests: std::sync::Mutex::new(Vec::new()),
            answer,
        })
    }

    pub(crate) fn requests(&self) -> Vec<ApprovalRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl ApprovalGate for RecordingGate {
    async fn request(&self, request: &ApprovalRequest) -> bool {
        self.requests.lock().unwrap().push(request.clone());
        self.answer
    }
}
