//! JSON-RPC 2.0 primitives — the framing MCP rides on.
//!
//! MCP's stdio transport is newline-delimited JSON-RPC 2.0: one request,
//! response, or notification per line, `id` correlating requests with
//! responses, and notifications (`id: null`) never answered.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One inbound JSON-RPC message (request or notification).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub jsonrpc: String,
    /// `null` for notifications; requests carry a number or string.
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

/// A successful JSON-RPC response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: Value,
    pub result: Value,
}

/// A JSON-RPC error response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub jsonrpc: String,
    pub id: Value,
    pub error: RpcError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;

    pub fn method_not_found() -> Self {
        Self { code: Self::METHOD_NOT_FOUND, message: "Method not found".to_string() }
    }

    pub fn parse_error() -> Self {
        Self { code: Self::PARSE_ERROR, message: "Parse error".to_string() }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self { code: Self::INVALID_PARAMS, message: message.into() }
    }
}

/// Parse one line into a JSON-RPC message. Notifications come back with
/// `id: None` and should never be answered.
pub fn parse(line: &str) -> Result<Message, RpcError> {
    let value: Value = serde_json::from_str(line).map_err(|_| RpcError::parse_error())?;
    let method = value.get("method").and_then(|m| m.as_str()).ok_or_else(|| {
        RpcError { code: RpcError::INVALID_REQUEST, message: "missing method".to_string() }
    })?;
    let id = value.get("id").cloned();
    Ok(Message {
        jsonrpc: "2.0".to_string(),
        id,
        method: method.to_string(),
        params: value.get("params").cloned(),
    })
}

pub fn success(id: &Value, result: Value) -> String {
    serde_json::to_string(&Response {
        jsonrpc: "2.0".to_string(),
        id: id.clone(),
        result,
    })
    .unwrap_or_else(|_| {
        serde_json::json!({
            "jsonrpc": "2.0", "id": id,
            "error": {"code": -32603, "message": "internal error serializing response"}
        })
        .to_string()
    })
}

pub fn error(id: &Value, err: &RpcError) -> String {
    serde_json::to_string(&ErrorResponse {
        jsonrpc: "2.0".to_string(),
        id: id.clone(),
        error: err.clone(),
    })
    .unwrap_or_else(|_| {
        serde_json::json!({
            "jsonrpc": "2.0", "id": id,
            "error": {"code": -32603, "message": "internal error serializing error"}
        })
        .to_string()
    })
}

pub fn notification(method: &str, params: Value) -> String {
    serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_and_notification() {
        let msg = parse(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"x"}}"#)
            .unwrap();
        assert_eq!(msg.id, Some(serde_json::json!(1)));
        assert_eq!(msg.method, "tools/call");
        assert_eq!(msg.params.unwrap()["name"], "x");

        let note = parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap();
        assert!(note.id.is_none(), "notifications carry no id");
    }

    #[test]
    fn parse_error_on_garbage() {
        let err = parse("this is not json").unwrap_err();
        assert_eq!(err.code, RpcError::PARSE_ERROR);
    }

    #[test]
    fn responses_are_newline_free_and_parseable() {
        let out = success(&serde_json::json!(7), serde_json::json!({"tools": []}));
        assert!(!out.contains('\n'), "stdio framing is one message per line");
        let back: Response = serde_json::from_str(&out).unwrap();
        assert_eq!(back.id, serde_json::json!(7));
        assert_eq!(back.result["tools"], serde_json::json!([]));
    }
}
