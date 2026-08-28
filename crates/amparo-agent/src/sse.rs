//! OpenAI-shaped SSE stream → one assembled assistant turn.
//!
//! `complete_chat_stream` returns a byte stream of `data: {...}` frames
//! terminated by `data: [DONE]`. Tool-call deltas arrive fragmented across
//! frames (`index` + `id`, then `function.name`, then `function.arguments`
//! piece by piece), so the accumulator concatenates by index. Some local
//! providers drop the `[DONE]` terminator — the accumulator finalizes from
//! whatever arrived when the stream ends.

use amparo_inference::{AssistantToolCall, FunctionCall, InferenceError, InferenceStream};
use futures_util::StreamExt;

/// One assembled assistant turn.
#[derive(Debug, Clone, Default)]
pub struct SseTurn {
    pub content: String,
    pub tool_calls: Vec<AssistantToolCall>,
    pub finish_reason: Option<String>,
}

impl SseTurn {
    pub fn is_empty(&self) -> bool {
        self.content.trim().is_empty() && self.tool_calls.is_empty()
    }
}

/// One tool-call delta in progress; `arguments` arrive as string fragments.
#[derive(Debug, Default)]
struct AccumCall {
    index: usize,
    id: Option<String>,
    name: String,
    arguments: String,
}

/// Sanity bound — a stream claiming thousands of tool-call indices is not
/// something to allocate for.
const MAX_TOOL_CALLS: usize = 64;

fn apply_delta(line: &str, turn: &mut SseTurn, calls: &mut Vec<AccumCall>) {
    let value: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return, // a malformed frame is skipped, not fatal
    };
    let Some(choice) = value.get("choices").and_then(|c| c.get(0)) else {
        return;
    };
    if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str()) {
        turn.finish_reason = Some(fr.to_string());
    }
    let Some(delta) = choice.get("delta") else { return };
    if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
        turn.content.push_str(text);
    }
    let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) else {
        return;
    };
    for tc in tcs {
        let index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        if index >= MAX_TOOL_CALLS {
            continue;
        }
        while calls.len() <= index {
            calls.push(AccumCall::default());
        }
        let call = &mut calls[index];
        call.index = index;
        if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
            call.id = Some(id.to_string());
        }
        if let Some(f) = tc.get("function") {
            if let Some(n) = f.get("name").and_then(|v| v.as_str()) {
                call.name.push_str(n);
            }
            if let Some(a) = f.get("arguments").and_then(|v| v.as_str()) {
                call.arguments.push_str(a);
            }
        }
    }
}

fn finalize_calls(calls: Vec<AccumCall>) -> Vec<AssistantToolCall> {
    calls
        .into_iter()
        .filter(|c| !c.name.is_empty())
        .map(|c| AssistantToolCall {
            id: c.id.unwrap_or_else(|| format!("call_{}", c.index)),
            call_type: "function".to_string(),
            function: FunctionCall { name: c.name, arguments: c.arguments },
        })
        .collect()
}

/// Consume an `InferenceStream` and assemble the assistant turn.
///
/// A `data: [DONE]` frame ends the turn; a stream that ends without one is
/// finalized from whatever arrived (some local providers omit it). Stream
/// errors propagate — a turn that failed mid-flight must not look like an
/// empty one.
pub async fn accumulate_turn(mut stream: InferenceStream) -> Result<SseTurn, InferenceError> {
    let mut turn = SseTurn::default();
    let mut calls: Vec<AccumCall> = Vec::new();
    let mut buf = String::new();
    while let Some(item) = stream.next().await {
        let bytes = item?;
        buf.push_str(&String::from_utf8_lossy(&bytes));
        while let Some(nl) = buf.find('\n') {
            let line: String = buf.drain(..=nl).collect();
            let data = line
                .trim_end()
                .strip_prefix("data:")
                .map(|d| d.trim())
                .unwrap_or("");
            if data.is_empty() {
                continue;
            }
            if data == "[DONE]" {
                turn.tool_calls = finalize_calls(calls);
                return Ok(turn);
            }
            apply_delta(data, &mut turn, &mut calls);
        }
    }
    // Stream ended without [DONE] — finalize with whatever arrived. An empty
    // turn is a legitimate outcome here; the agent loop's empty-turn
    // recovery decides what to do with it.
    turn.tool_calls = finalize_calls(calls);
    Ok(turn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    fn stream_of<S: AsRef<str>>(frames: &[S]) -> InferenceStream {
        let events: Vec<std::result::Result<bytes::Bytes, InferenceError>> = frames
            .iter()
            .map(|f| Ok(bytes::Bytes::from(format!("data: {}\n\n", f.as_ref()))))
            .collect();
        Box::pin(stream::iter(events))
    }

    fn delta(value: serde_json::Value) -> String {
        serde_json::json!({"choices": [{"delta": value}]}).to_string()
    }

    #[tokio::test]
    async fn content_deltas_concatenate_until_done() {
        let frames = [
            delta(serde_json::json!({"content": "Hel"})),
            delta(serde_json::json!({"content": "lo"})),
            "[DONE]".to_string(),
        ];
        let turn = accumulate_turn(stream_of(&frames)).await.unwrap();
        assert_eq!(turn.content, "Hello");
        assert!(turn.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn tool_call_fragments_assemble_by_index() {
        let frames = [
            delta(serde_json::json!({"tool_calls": [{
                "index": 0, "id": "call_x", "type": "function",
                "function": {"name": "run_", "arguments": ""}
            }]})),
            delta(serde_json::json!({"tool_calls": [{
                "index": 0, "function": {"name": "command", "arguments": "{\"command\":\"ls\""}
            }]})),
            delta(serde_json::json!({"tool_calls": [{
                "index": 0, "function": {"arguments": "}"}
            }]})),
            "[DONE]".to_string(),
        ];
        let turn = accumulate_turn(stream_of(&frames)).await.unwrap();
        assert_eq!(turn.tool_calls.len(), 1);
        let call = &turn.tool_calls[0];
        assert_eq!(call.id, "call_x");
        assert_eq!(call.call_type, "function");
        assert_eq!(call.function.name, "run_command");
        assert_eq!(call.function.arguments, "{\"command\":\"ls\"}");
    }

    #[tokio::test]
    async fn stream_without_done_finalizes_partial() {
        let frames = [
            delta(serde_json::json!({"content": "partial"})),
            // no [DONE] — the stream just ends
        ];
        let turn = accumulate_turn(stream_of(&frames)).await.unwrap();
        assert_eq!(turn.content, "partial");
    }

    #[tokio::test]
    async fn empty_stream_is_an_empty_turn() {
        let frames: [&str; 0] = [];
        let turn = accumulate_turn(stream_of(&frames)).await.unwrap();
        assert!(turn.is_empty());
    }

    #[tokio::test]
    async fn stream_error_propagates() {
        let stream: InferenceStream = Box::pin(stream::once(async {
            Err(InferenceError::Provider("dropped".into()))
        }));
        let err = accumulate_turn(stream).await.unwrap_err();
        assert!(err.to_string().contains("dropped"));
    }

    #[tokio::test]
    async fn malformed_and_comment_frames_are_skipped() {
        let frames = [
            "not json".to_string(),
            "keep-alive comment without data prefix".to_string(),
            delta(serde_json::json!({"content": "ok"})),
            "[DONE]".to_string(),
        ];
        let turn = accumulate_turn(stream_of(&frames)).await.unwrap();
        assert_eq!(turn.content, "ok");
    }

    #[tokio::test]
    async fn absurd_tool_call_index_is_ignored() {
        let frames = [
            delta(serde_json::json!({"tool_calls": [{
                "index": 100000, "id": "boom", "type": "function",
                "function": {"name": "rm", "arguments": "{}"}
            }]})),
            "[DONE]".to_string(),
        ];
        let turn = accumulate_turn(stream_of(&frames)).await.unwrap();
        assert!(turn.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn finish_reason_is_captured() {
        let frames = [
            delta(serde_json::json!({"content": "done"})),
            serde_json::json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}).to_string(),
            "[DONE]".to_string(),
        ];
        let turn = accumulate_turn(stream_of(&frames)).await.unwrap();
        assert_eq!(turn.finish_reason.as_deref(), Some("stop"));
    }
}
