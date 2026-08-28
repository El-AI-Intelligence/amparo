//! MCP protocol shapes — just the tool-related slice of the spec.
//!
//! MCP is JSON-RPC 2.0 with a defined vocabulary; this crate implements the
//! `initialize` handshake, `tools/list`, `tools/call`, and `ping`. Resources,
//! prompts, elicitation, and the rest of the spec surface stay out of scope
//! for the Amparo integration.

use amparo_tools::{ToolParam, ToolSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: &str = "2025-06-18";

// ── Handshake ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientCapabilities {
    #[serde(default)]
    pub tools: Option<serde_json::Value>,
}

// The camelCase field names below are the MCP wire protocol's literal JSON
// keys (protocolVersion, clientInfo, listChanged, inputSchema, isError) —
// kept verbatim so serialization needs no mapping layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(non_snake_case)]
pub struct InitializeRequest {
    pub protocolVersion: String,
    pub capabilities: ClientCapabilities,
    pub clientInfo: ClientInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerCapabilities {
    pub tools: ServerToolsCapabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(non_snake_case)]
pub struct ServerToolsCapabilities {
    #[serde(default)]
    pub listChanged: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(non_snake_case)]
pub struct InitializeResult {
    pub protocolVersion: String,
    pub capabilities: ServerCapabilities,
    pub serverInfo: ClientInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

// ── Tools ────────────────────────────────────────────────────────────────────

/// MCP's tool shape: name, description, and a JSON Schema inputSchema.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(non_snake_case)]
pub struct McpTool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub inputSchema: McpToolSchema,
}

/// The subset of JSON Schema MCP tools use: top-level object with typed
/// properties and a required list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolSchema {
    #[serde(rename = "type")]
    pub schema_type: String,
    #[serde(default)]
    pub properties: serde_json::Map<String, Value>,
    #[serde(default)]
    pub required: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListToolsResult {
    pub tools: Vec<McpTool>,
}

/// One content block in a `tools/call` result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(non_snake_case)]
pub struct CallToolResult {
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub isError: bool,
}

// ── Conversion: registry schema ⇄ MCP tool ───────────────────────────────────

/// Render a registry tool schema as an MCP tool definition.
pub fn to_mcp_tool(schema: &ToolSchema) -> McpTool {
    let mut properties = serde_json::Map::new();
    for p in &schema.parameters {
        let mut obj = serde_json::Map::new();
        obj.insert("type".to_string(), Value::String(mcp_type(&p.param_type)));
        if !p.description.is_empty() {
            obj.insert("description".to_string(), Value::String(p.description.clone()));
        }
        if let Some(enums) = &p.enum_values {
            obj.insert(
                "enum".to_string(),
                Value::Array(enums.iter().map(|e| Value::String(e.clone())).collect()),
            );
        }
        properties.insert(p.name.clone(), Value::Object(obj));
    }
    McpTool {
        name: schema.name.clone(),
        description: schema.description.clone(),
        inputSchema: McpToolSchema {
            schema_type: "object".to_string(),
            properties,
            required: schema
                .parameters
                .iter()
                .filter(|p| p.required)
                .map(|p| p.name.clone())
                .collect(),
        },
    }
}

/// Map an amparo parameter type onto a JSON Schema type. Amparo params keep
/// their wire names (`array`/`object` stay, `bool` becomes `boolean`).
fn mcp_type(param_type: &str) -> String {
    match param_type {
        "bool" => "boolean".to_string(),
        other => other.to_string(),
    }
}

/// Interpret an MCP tool's inputSchema as amparo registry parameters.
///
/// Only the top-level `type: object` shape is understood; nested schemas
/// degrade to their JSON-Schema type name. Parameters the schema cannot
/// express (no `type` key) are skipped rather than guessed.
pub fn mcp_tool_params(tool: &McpTool) -> Vec<ToolParam> {
    tool.inputSchema
        .properties
        .iter()
        .filter_map(|(name, prop)| {
            let obj = prop.as_object()?;
            let param_type = obj.get("type")?.as_str()?.to_string();
            Some(ToolParam {
                name: name.clone(),
                description: obj
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                param_type,
                enum_values: obj.get("enum").and_then(|e| {
                    let values: Vec<String> = e
                        .as_array()?
                        .iter()
                        .map(|v| match v {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .collect();
                    Some(values)
                }),
                required: tool.inputSchema.required.contains(name),
            })
        })
        .collect()
}

/// Extract the first text block from a `tools/call` result — the portable
/// summary both the agent loop and the MCP server display.
pub fn first_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .find(|c| c.block_type == "text")
        .map(|c| c.text.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use amparo_tools::ToolTrustTier;

    fn schema() -> ToolSchema {
        ToolSchema {
            name: "read_file".to_string(),
            description: "read a file".to_string(),
            parameters: vec![ToolParam {
                name: "path".to_string(),
                description: "file path".to_string(),
                param_type: "string".to_string(),
                enum_values: None,
                required: true,
            }],
            trust_tier: ToolTrustTier::Observational,
        }
    }

    #[test]
    fn registry_schema_round_trips_through_mcp() {
        let mcp = to_mcp_tool(&schema());
        assert_eq!(mcp.name, "read_file");
        assert_eq!(mcp.inputSchema.schema_type, "object");
        assert_eq!(mcp.inputSchema.required, vec!["path"]);
        assert_eq!(mcp.inputSchema.properties["path"]["type"], "string");

        let params = mcp_tool_params(&mcp);
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].name, "path");
        assert_eq!(params[0].param_type, "string");
        assert!(params[0].required);
    }

    #[test]
    fn mcp_tool_params_handles_enums_and_missing_types() {
        let mcp = McpTool {
            name: "x".into(),
            description: String::new(),
            inputSchema: McpToolSchema {
                schema_type: "object".into(),
                properties: serde_json::json!({
                    "mode": {"type": "string", "enum": ["overwrite", "append"]},
                    "mystery": {"description": "no type"}
                })
                .as_object()
                .unwrap()
                .clone(),
                required: vec!["mode".into()],
            },
        };
        let params = mcp_tool_params(&mcp);
        assert_eq!(params.len(), 1, "properties without a type are skipped");
        assert_eq!(params[0].enum_values.as_deref(), Some(&["overwrite".into(), "append".into()][..]));
    }

    #[test]
    fn first_text_extracts_from_content_blocks() {
        let result = CallToolResult {
            content: vec![
                ContentBlock { block_type: "text".into(), text: "hello".into() },
                ContentBlock { block_type: "image".into(), text: String::new() },
            ],
            isError: false,
        };
        assert_eq!(first_text(&result), "hello");
    }
}
