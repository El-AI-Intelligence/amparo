//! The `eval_wasm` tool — the model-facing front door to the sandbox (M7b).

use crate::{SandboxRuntime, OUTPUT_BASE, OUTPUT_CAP};
use amparo_tools::{ToolCall, ToolExecutor, ToolParam, ToolResult, ToolSchema, ToolTrustTier};
use async_trait::async_trait;
use std::time::Instant;

/// The name of the sandbox-eval tool, as exposed to the LLM.
pub const EVAL_WASM: &str = "eval_wasm";

/// The `eval_wasm` tool — executes an untrusted WASM module under the
/// sandbox bounds.
///
/// The module arrives base64-encoded (LLMs cannot emit raw bytes) and must
/// obey the ABI contract stated in the schema description. Trust tier is
/// [`ToolTrustTier::ExternalEffector`]: executing untrusted code always
/// asks a human, even though the sandbox keeps the reach small. Every
/// failure path — missing parameter, bad base64, compile error, fuel
/// exhaustion, timeout — returns a failed [`ToolResult`] with an
/// explanatory message; this executor never panics.
pub struct EvalWasmTool {
    runtime: SandboxRuntime,
}

impl Default for EvalWasmTool {
    fn default() -> Self {
        Self::new()
    }
}

impl EvalWasmTool {
    /// Creates an `eval_wasm` tool over the default sandbox limits
    /// (10M fuel, 4 MB module, 4 MB memory, 30 s wall clock).
    pub fn new() -> Self {
        Self {
            runtime: SandboxRuntime::new(),
        }
    }

    /// Creates an `eval_wasm` tool over a caller-configured runtime
    /// (narrowed limits, test fixtures).
    pub fn with_runtime(runtime: SandboxRuntime) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl ToolExecutor for EvalWasmTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: EVAL_WASM.to_string(),
            description: format!(
                "Execute an untrusted WebAssembly module in a fuel-metered \
                 sandbox and return its JSON output. The module must export \
                 `memory` and `axiom_eval(i32 input_ptr, i32 input_len, i32 \
                 output_ptr, i32 output_cap) -> i32`: it receives the input \
                 string at memory offset 0, writes its JSON output at \
                 output_ptr ({}), and returns the number of bytes written \
                 (>= 0) or -1 for an error. The module may import nothing \
                 and runs with no WASI, no filesystem, no network, and no \
                 clock. Limits: 10M fuel instructions, 4 MB module, 4 MB \
                 memory, 30 s wall clock; output must fit in {} bytes. \
                 Execution is deterministic: the same module and input \
                 always produce the same output.",
                OUTPUT_BASE, OUTPUT_CAP
            ),
            parameters: vec![
                ToolParam {
                    name: "wasm_base64".to_string(),
                    description: "The WASM module to run, base64-encoded.".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: true,
                },
                ToolParam {
                    name: "input".to_string(),
                    description: "Input JSON string passed to the module at \
                                  memory offset 0 (default \"{}\")."
                        .to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: false,
                },
            ],
            trust_tier: ToolTrustTier::ExternalEffector,
        }
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        let start = Instant::now();

        let Some(wasm_b64) = call.arg_str("wasm_base64") else {
            return failure(
                call,
                "eval_wasm requires a wasm_base64 string parameter",
                start,
            );
        };
        let input = call.arg_str("input").unwrap_or("{}").to_string();

        // The eval blocks a dedicated worker thread (and enforces its own
        // wall-clock timeout); run it off the async executor.
        let runtime = self.runtime.clone();
        let wasm_b64 = wasm_b64.to_string();
        match tokio::task::spawn_blocking(move || runtime.eval_b64(&wasm_b64, &input)).await {
            Ok(Ok(result)) => ToolResult {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                success: true,
                output: serde_json::json!({
                    "output": result.output,
                    "elapsed_ms": result.elapsed_ms,
                    "fuel_consumed": result.fuel_consumed,
                    "memory_used": result.memory_used,
                }),
                display_summary: format!(
                    "eval_wasm completed: {} ms, {} fuel, {} bytes of memory",
                    result.elapsed_ms, result.fuel_consumed, result.memory_used
                ),
                duration_ms: start.elapsed().as_millis() as u64,
            },
            Ok(Err(e)) => failure(call, &format!("eval_wasm failed: {e}"), start),
            Err(join) => failure(
                call,
                &format!("eval_wasm worker panicked or was cancelled: {join}"),
                start,
            ),
        }
    }
}

/// Build the failed [`ToolResult`] shared by every error path.
fn failure(call: &ToolCall, message: &str, start: Instant) -> ToolResult {
    ToolResult {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        success: false,
        output: serde_json::json!({ "error": message }),
        display_summary: message.to_string(),
        duration_ms: start.elapsed().as_millis() as u64,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use amparo_tools::registry::default_registry;

    /// The W1 echo module: copies the input region into the output region
    /// and returns the byte count.
    fn echo_wasm_b64() -> String {
        let wasm = wat::parse_str(
            r#"(module
            (memory (export "memory") 8)
            (func (export "axiom_eval") (param $in_ptr i32) (param $in_len i32) (param $out_ptr i32) (param $out_cap i32) (result i32)
                (local $i i32)
                (block $done
                    (loop $copy
                        (br_if $done (i32.ge_u (local.get $i) (local.get $in_len)))
                        (i32.store8 (i32.add (local.get $out_ptr) (local.get $i))
                                    (i32.load8_u (i32.add (local.get $in_ptr) (local.get $i))))
                        (local.set $i (i32.add (local.get $i) (i32.const 1)))
                        (br $copy)))
                (local.get $in_len)))"#,
        )
        .expect("valid WAT");
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(wasm)
    }

    /// An infinite loop — dies by fuel exhaustion, never by hang.
    fn looping_wasm_b64() -> String {
        let wasm = wat::parse_str(
            r#"(module
            (memory (export "memory") 1)
            (func (export "axiom_eval") (param i32 i32 i32 i32) (result i32)
                (loop (br 0))
                (i32.const 0)))"#,
        )
        .expect("valid WAT");
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(wasm)
    }

    fn call(wasm: Option<&str>) -> ToolCall {
        let arguments = match wasm {
            Some(b64) => serde_json::json!({"wasm_base64": b64}),
            None => serde_json::json!({}),
        };
        ToolCall {
            id: "1".to_string(),
            name: EVAL_WASM.to_string(),
            arguments,
        }
    }

    #[test]
    fn schema_states_the_contract() {
        let schema = EvalWasmTool::new().schema();
        assert_eq!(schema.name, EVAL_WASM);
        assert_eq!(schema.trust_tier, ToolTrustTier::ExternalEffector);
        assert_eq!(schema.parameters.len(), 2);
        let wasm = &schema.parameters[0];
        assert_eq!(wasm.name, "wasm_base64");
        assert_eq!(wasm.param_type, "string");
        assert!(wasm.required);
        let input = &schema.parameters[1];
        assert_eq!(input.name, "input");
        assert_eq!(input.param_type, "string");
        assert!(!input.required);
        for contract in ["axiom_eval", "import nothing", "10M fuel", "deterministic"] {
            assert!(
                schema.description.contains(contract),
                "description must state the contract ({contract}): {}",
                schema.description
            );
        }
    }

    #[tokio::test]
    async fn execute_round_trips_the_module() {
        let tool = EvalWasmTool::new();
        let result = tool.execute(&call(Some(&echo_wasm_b64()))).await;
        assert!(result.success, "round trip must succeed: {result:?}");
        assert_eq!(result.output["output"], serde_json::json!({}));
        assert!(result.output["fuel_consumed"].as_u64().unwrap() > 0);
        assert!(result.display_summary.contains("eval_wasm completed"));
    }

    #[tokio::test]
    async fn execute_passes_the_input_through() {
        let tool = EvalWasmTool::new();
        let mut c = call(Some(&echo_wasm_b64()));
        c.arguments = serde_json::json!({
            "wasm_base64": echo_wasm_b64(),
            "input": "{\"msg\":\"hello\"}"
        });
        let result = tool.execute(&c).await;
        assert!(result.success, "input round trip must succeed: {result:?}");
        assert_eq!(result.output["output"], serde_json::json!({"msg": "hello"}));
    }

    #[tokio::test]
    async fn execute_fails_without_the_parameter() {
        let result = EvalWasmTool::new().execute(&call(None)).await;
        assert!(!result.success);
        assert!(result.output["error"]
            .as_str()
            .unwrap()
            .contains("wasm_base64"));
    }

    #[tokio::test]
    async fn execute_fails_on_garbage_base64() {
        let result = EvalWasmTool::new()
            .execute(&call(Some("!!!not-base64!!!")))
            .await;
        assert!(!result.success);
        assert!(result.output["error"].as_str().unwrap().contains("base64"));
    }

    #[tokio::test]
    async fn execute_dies_by_fuel_not_hang() {
        let result = EvalWasmTool::new()
            .execute(&call(Some(&looping_wasm_b64())))
            .await;
        assert!(!result.success);
        let msg = result.output["error"].as_str().unwrap();
        assert!(msg.contains("fuel"), "must be a fuel failure: {msg}");
    }

    #[test]
    fn default_registry_stays_at_20_without_eval_wasm() {
        // eval_wasm is registered host-side only — the default registry
        // is untouched by the sandbox crate. (20 = the 17 base tools
        // plus the blackboard pair and send_notification, M10.)
        let reg = default_registry();
        let schemas = reg.list_schemas();
        assert_eq!(schemas.len(), 20, "default registry count changed");
        assert!(
            schemas.iter().all(|s| s.name != EVAL_WASM),
            "eval_wasm must not be in the default registry"
        );
    }
}
