// Originally part of Axiom-OS (MIT, Copyright (c) Pixel Phantom AI) —
// `crates/axiom-sandbox`, the WASM eval sandbox (M3.4).
// Ported to Amparo and relicensed Apache-2.0 — see the repository NOTICE.
//
// Amparo's port keeps the fuel-metered runtime and drops the rest: the
// Python/JavaScript/Rust source compilers are placeholders that fabricate
// success, the HTTP route belongs to a daemon (Amparo ships a library),
// and WASI stays out entirely.

//! A fuel-metered, deterministic WASM execution sandbox (M7b).
//!
//! [`SandboxRuntime`] executes untrusted WebAssembly modules under hard
//! bounds — instruction fuel, module size, linear-memory size, and
//! wall-clock time — with **no imports and no WASI**: a module that
//! passes validation can compute, and nothing else. The module contract:
//!
//! - the module exports `memory` and
//!   `axiom_eval(i32 input_ptr, i32 input_len, i32 output_ptr, i32
//!   output_cap) -> i32`;
//! - the input JSON is written at offset 0; the module writes its
//!   output at `output_ptr` and returns the number of bytes written
//!   (>= 0), or -1 for an error;
//! - output must fit inside the output region ([`OUTPUT_BASE`],
//!   [`OUTPUT_CAP`]); input must fit inside the module's memory and the
//!   input region (0 .. [`OUTPUT_BASE`]) — the two regions never
//!   overlap.
//!
//! Fuel is the hard bound — an infinite loop dies by fuel exhaustion —
//! and the wall-clock timeout is belt-and-braces: the eval runs on a
//! dedicated thread, the caller times out and joins (the join itself is
//! bounded by fuel). A module with any import is rejected at compile
//! time, so there is no host surface to escape through. With no
//! imports, no WASI, and a single-threaded store, the same module and
//! input always produce the same output (up to NaN payloads).

#![warn(missing_docs)]

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

pub mod tool;
pub use tool::{EvalWasmTool, EVAL_WASM};

/// Byte offset where the module must write its output — 256 KB, the
/// start of the output region.
pub const OUTPUT_BASE: u32 = 256 * 1024;
/// Maximum output size in bytes — the output region is
/// [`OUTPUT_BASE`] .. [`OUTPUT_BASE`] + [`OUTPUT_CAP`].
pub const OUTPUT_CAP: u32 = 256 * 1024;

const DEFAULT_FUEL_LIMIT: u64 = 10_000_000;
const DEFAULT_MAX_MODULE_BYTES: usize = 4 * 1024 * 1024; // 4 MB
const DEFAULT_MAX_MEMORY_BYTES: usize = 4 * 1024 * 1024; // 4 MB
const DEFAULT_TIMEOUT_MS: u64 = 30_000; // 30 s
const DEFAULT_MAX_CONCURRENT: usize = 4;

// ── Errors ────────────────────────────────────────────────────────────────────

/// Errors that can occur during sandbox operations.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// The binary is not structurally valid WASM (magic, version,
    /// length).
    #[error("invalid WASM binary: {0}")]
    InvalidWasm(String),
    /// The base64 module payload did not decode.
    #[error("base64 decode error: {0}")]
    Base64Decode(String),
    /// Execution failed: a trap, a module error, or a contract
    /// violation.
    #[error("execution failed: {0}")]
    ExecutionFailed(String),
    /// The module exhausted its instruction fuel.
    #[error("fuel exhausted after {0} instructions")]
    FuelExhausted(u64),
    /// The input does not fit inside the module's memory.
    #[error("memory limit exceeded ({0} bytes requested, {1} allowed)")]
    MemoryExceeded(usize, usize),
    /// The module binary exceeds the size limit.
    #[error("module too large ({0} bytes, limit {1})")]
    ModuleTooLarge(usize, usize),
    /// Compilation failed.
    #[error("compilation failed: {0}")]
    CompilationFailed(String),
    /// The wall-clock timeout fired; the worker thread is joined after
    /// (bounded by fuel).
    #[error("execution timeout after {0}ms")]
    ExecutionTimeout(u64),
    /// A wasmtime engine/store operation failed.
    #[error("wasmtime error: {0}")]
    WasmtimeError(String),
    /// The module declares imports, which the sandbox rejects: there is
    /// no host surface to import from.
    #[error("imports are rejected in the sandbox: {0}")]
    ImportRejected(String),
    /// The input exceeds the input region (0 .. [`OUTPUT_BASE`]).
    #[error("input too large ({0} bytes; the input region is {1} bytes)")]
    InputTooLarge(usize, usize),
    /// The sandbox worker thread failed to start or report.
    #[error("sandbox worker failed: {0}")]
    WorkerFailed(String),
}

// ── Result ────────────────────────────────────────────────────────────────────

/// Output from a sandbox evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SandboxResult {
    /// Structured output: the parsed output-region bytes, or a
    /// `raw_output` fallback when the module's output is not JSON.
    pub output: serde_json::Value,
    /// Wall-clock time in milliseconds, from the caller's start to the
    /// worker's report.
    pub elapsed_ms: u64,
    /// Fuel consumed (instruction budget spent).
    pub fuel_consumed: u64,
    /// Whether actual execution occurred — always `true` on `Ok`.
    pub executed: bool,
    /// The module's linear-memory size in bytes after the call.
    pub memory_used: usize,
}

// ── Runtime ───────────────────────────────────────────────────────────────────

/// WASM sandbox runtime with a wasmtime execution engine.
///
/// Provides deterministic, fuel-metered execution of WASM modules under
/// four bounds: instruction fuel, module size, linear-memory size, and
/// wall-clock time. Thread-safe: `Arc<SandboxRuntime>` is the intended
/// sharing pattern; the concurrency semaphore caps parallel evals.
#[derive(Clone)]
pub struct SandboxRuntime {
    /// Max instructions before forcible abort (per eval call).
    pub fuel_limit: u64,
    /// Maximum WASM binary size in bytes.
    pub max_module_bytes: usize,
    /// Maximum linear memory per module in bytes.
    pub max_memory_bytes: usize,
    /// Wall-clock execution timeout in milliseconds (belt-and-braces —
    /// fuel is the hard bound).
    pub timeout_ms: u64,
    concurrency: Arc<Semaphore>,
}

impl Default for SandboxRuntime {
    fn default() -> Self {
        Self {
            fuel_limit: DEFAULT_FUEL_LIMIT,
            max_module_bytes: DEFAULT_MAX_MODULE_BYTES,
            max_memory_bytes: DEFAULT_MAX_MEMORY_BYTES,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            concurrency: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT)),
        }
    }
}

impl SandboxRuntime {
    /// Create a sandbox runtime with default limits.
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the instruction fuel limit.
    pub fn with_fuel_limit(mut self, limit: u64) -> Self {
        self.fuel_limit = limit;
        self
    }

    /// Override the maximum module size.
    pub fn with_max_module_bytes(mut self, bytes: usize) -> Self {
        self.max_module_bytes = bytes;
        self
    }

    /// Override the maximum linear-memory size.
    pub fn with_max_memory_bytes(mut self, bytes: usize) -> Self {
        self.max_memory_bytes = bytes;
        self
    }

    /// Override the wall-clock execution timeout.
    pub fn with_timeout(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    /// Override max concurrent executions.
    pub fn with_max_concurrent(mut self, max: usize) -> Self {
        self.concurrency = Arc::new(Semaphore::new(max));
        self
    }

    /// Validate a raw WASM binary (size guard, magic bytes, version 1).
    pub fn validate(&self, wasm_bytes: &[u8]) -> Result<(), SandboxError> {
        if wasm_bytes.len() > self.max_module_bytes {
            return Err(SandboxError::ModuleTooLarge(
                wasm_bytes.len(),
                self.max_module_bytes,
            ));
        }
        // Minimum size: 4-byte magic + 4-byte version.
        if wasm_bytes.len() < 8 {
            return Err(SandboxError::InvalidWasm(
                "binary is too short to be a valid WASM module (< 8 bytes)".into(),
            ));
        }
        // WASM magic: \0asm.
        if &wasm_bytes[0..4] != b"\0asm" {
            return Err(SandboxError::InvalidWasm(
                "missing WASM magic bytes (expected 0x00 0x61 0x73 0x6d)".into(),
            ));
        }
        // WASM version must be 1.
        let version = u32::from_le_bytes(
            wasm_bytes[4..8]
                .try_into()
                .expect("slice length guaranteed above"),
        );
        if version != 1 {
            return Err(SandboxError::InvalidWasm(format!(
                "unsupported WASM version: {} (only version 1 supported)",
                version
            )));
        }
        Ok(())
    }

    /// Evaluate a raw WASM binary with JSON input.
    ///
    /// Runs the module under the ABI contract on a dedicated worker
    /// thread. This method blocks the calling thread until the eval
    /// finishes or `timeout_ms` elapses; callers on a tokio runtime
    /// should wrap it in `tokio::task::spawn_blocking`. On timeout the
    /// worker is joined — the join completes when fuel kills the module
    /// (fuel is the hard bound; the timeout is UX).
    pub fn eval(&self, wasm_bytes: &[u8], input_json: &str) -> Result<SandboxResult, SandboxError> {
        let start = Instant::now();

        // Acquire the concurrency permit (held for the whole eval).
        let _permit = self.concurrency.try_acquire().map_err(|_| {
            SandboxError::ExecutionFailed("too many concurrent executions".into())
        })?;

        self.validate(wasm_bytes)?;
        let input = input_json.as_bytes().to_vec();
        let input_len = input.len();
        if input_len > OUTPUT_BASE as usize {
            return Err(SandboxError::InputTooLarge(input_len, OUTPUT_BASE as usize));
        }

        // Snapshot the config for the worker thread.
        let fuel_limit = self.fuel_limit;
        let max_memory_bytes = self.max_memory_bytes;
        let wasm = wasm_bytes.to_vec();
        let timeout = Duration::from_millis(self.timeout_ms);

        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("amparo-sandbox-eval".into())
            .spawn(move || {
                let result = run_eval(&wasm, &input, fuel_limit, max_memory_bytes);
                let _ = tx.send(result);
            })
            .map_err(|e| SandboxError::WorkerFailed(e.to_string()))?;

        match rx.recv_timeout(timeout) {
            Ok(result) => {
                // The result arrived; the join is immediate.
                let _ = worker.join();
                result.map(|mut r| {
                    r.elapsed_ms = start.elapsed().as_millis() as u64;
                    r
                })
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Fuel bounds the worker: it exits on its own shortly.
                let _ = worker.join();
                Err(SandboxError::ExecutionTimeout(self.timeout_ms))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let _ = worker.join();
                Err(SandboxError::WorkerFailed(
                    "sandbox worker ended without a result".into(),
                ))
            }
        }
    }

    /// Evaluate a base64-encoded WASM binary.
    pub fn eval_b64(&self, wasm_b64: &str, input_json: &str) -> Result<SandboxResult, SandboxError> {
        use base64::Engine as _;
        let wasm_bytes = base64::engine::general_purpose::STANDARD
            .decode(wasm_b64.trim())
            .map_err(|e| SandboxError::Base64Decode(e.to_string()))?;
        self.eval(&wasm_bytes, input_json)
    }
}

/// The worker-thread body: compile, bound, run, and report.
///
/// Everything wasmtime-related lives here — engines and stores are not
/// `Send`, and the worker owns them for its lifetime.
fn run_eval(
    wasm_bytes: &[u8],
    input: &[u8],
    fuel_limit: u64,
    max_memory_bytes: usize,
) -> Result<SandboxResult, SandboxError> {
    // Engine with fuel enabled; no WASI, no async.
    let mut config = wasmtime::Config::new();
    config.consume_fuel(true);
    config.max_wasm_stack(1024 * 1024); // 1 MB stack
    config.wasm_memory64(false);
    let engine = wasmtime::Engine::new(&config)
        .map_err(|e| SandboxError::WasmtimeError(e.to_string()))?;

    // Compile, then reject imports — there is no host surface to
    // import from, so any import is a sandbox violation, loudly
    // refused.
    let module = wasmtime::Module::new(&engine, wasm_bytes)
        .map_err(|e| SandboxError::CompilationFailed(e.to_string()))?;
    let import_count = module.imports().count();
    if import_count > 0 {
        let names: Vec<String> = module
            .imports()
            .map(|i| format!("{}.{}", i.module(), i.name()))
            .collect();
        return Err(SandboxError::ImportRejected(names.join(", ")));
    }

    // Refuse modules whose declared minimum memory already exceeds the
    // limit, before instantiation allocates it (the limiter would stop
    // it too — this is the clean error).
    for export in module.exports() {
        if let wasmtime::ExternType::Memory(memory) = export.ty() {
            let min_bytes = (memory.minimum() as usize).saturating_mul(64 * 1024);
            if min_bytes > max_memory_bytes {
                return Err(SandboxError::MemoryExceeded(min_bytes, max_memory_bytes));
            }
        }
    }

    // Store with fuel and memory limits. The limits live in the store
    // data: the limiter closure must be 'static and returns its
    // parameter, so a stack-local reference cannot satisfy it.
    let limits = wasmtime::StoreLimitsBuilder::new()
        .memory_size(max_memory_bytes)
        .instances(1)
        .build();
    let mut store = wasmtime::Store::new(&engine, limits);
    store
        .set_fuel(fuel_limit)
        .map_err(|e| SandboxError::WasmtimeError(e.to_string()))?;
    store.limiter(|limits| limits);

    let instance = wasmtime::Instance::new(&mut store, &module, &[])
        .map_err(|e| SandboxError::ExecutionFailed(e.to_string()))?;

    let memory = instance
        .get_memory(&mut store, "memory")
        .ok_or_else(|| SandboxError::ExecutionFailed("module must export 'memory'".into()))?;
    let eval_func = instance
        .get_func(&mut store, "axiom_eval")
        .ok_or_else(|| SandboxError::ExecutionFailed("module must export 'axiom_eval'".into()))?;

    // The input must fit inside the module's memory.
    if input.len() > memory.data_size(&store) {
        return Err(SandboxError::MemoryExceeded(input.len(), memory.data_size(&store)));
    }
    memory
        .write(&mut store, 0, input)
        .map_err(|e| SandboxError::ExecutionFailed(format!("memory write failed: {e}")))?;

    // axiom_eval(input_ptr, input_len, output_ptr, output_cap) -> i32
    let params = [
        wasmtime::Val::I32(0),
        wasmtime::Val::I32(input.len() as i32),
        wasmtime::Val::I32(OUTPUT_BASE as i32),
        wasmtime::Val::I32(OUTPUT_CAP as i32),
    ];
    let mut results = vec![wasmtime::Val::I32(0)];
    if let Err(trap) = eval_func.call(&mut store, &params, &mut results) {
        if store.get_fuel().map(|f| f == 0).unwrap_or(false) {
            let consumed = fuel_limit.saturating_sub(store.get_fuel().unwrap_or(0));
            return Err(SandboxError::FuelExhausted(consumed));
        }
        return Err(SandboxError::ExecutionFailed(format!("wasm trap: {trap}")));
    }

    // Memory growth beyond the limit is refused after the fact (the
    // limiter caps it at allocation; this is the hard post-check).
    let memory_used = memory.data_size(&store);
    if memory_used > max_memory_bytes {
        return Err(SandboxError::ExecutionFailed(format!(
            "memory grew to {} bytes, limit {}",
            memory_used, max_memory_bytes
        )));
    }

    let bytes_written = results[0].i32().unwrap_or(-1);
    if bytes_written < 0 {
        return Err(SandboxError::ExecutionFailed(format!(
            "module returned {bytes_written}"
        )));
    }
    if bytes_written as u32 > OUTPUT_CAP {
        return Err(SandboxError::ExecutionFailed(format!(
            "module returned {bytes_written} bytes; output cap is {OUTPUT_CAP}"
        )));
    }

    let output = if bytes_written == 0 {
        serde_json::Value::Null
    } else {
        let mut buf = vec![0u8; bytes_written as usize];
        memory
            .read(&store, OUTPUT_BASE as usize, &mut buf)
            .map_err(|e| SandboxError::ExecutionFailed(format!("memory read failed: {e}")))?;
        let output_str = String::from_utf8_lossy(&buf);
        serde_json::from_str(&output_str).unwrap_or_else(|_| {
            serde_json::json!({ "raw_output": output_str.to_string() })
        })
    };

    let fuel_consumed = fuel_limit.saturating_sub(store.get_fuel().unwrap_or(0));
    Ok(SandboxResult {
        output,
        elapsed_ms: 0, // set by the caller on receipt
        fuel_consumed,
        executed: true,
        memory_used,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    /// A module that echoes its input region into the output region and
    /// returns the byte count. Memory: 8 pages (512 KB) covers the
    /// input region (0..256 KB) and the output region (256..512 KB).
    fn echo_wasm() -> Vec<u8> {
        wat::parse_str(
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
        .expect("valid WAT")
    }

    /// Minimal valid WASM: magic + version 1, no sections.
    fn minimal_wasm() -> Vec<u8> {
        vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]
    }

    #[test]
    fn validate_accepts_minimal_wasm() {
        let rt = SandboxRuntime::new();
        assert!(rt.validate(&minimal_wasm()).is_ok());
    }

    #[test]
    fn validate_rejects_wrong_magic() {
        let rt = SandboxRuntime::new();
        let bad = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x00, 0x00, 0x00];
        assert!(matches!(rt.validate(&bad), Err(SandboxError::InvalidWasm(_))));
    }

    #[test]
    fn validate_rejects_wrong_version() {
        let rt = SandboxRuntime::new();
        let bad = vec![0x00, 0x61, 0x73, 0x6d, 0x02, 0x00, 0x00, 0x00];
        assert!(matches!(rt.validate(&bad), Err(SandboxError::InvalidWasm(_))));
    }

    #[test]
    fn validate_rejects_too_short() {
        let rt = SandboxRuntime::new();
        assert!(matches!(rt.validate(b"\0asm"), Err(SandboxError::InvalidWasm(_))));
    }

    #[test]
    fn validate_rejects_oversized_module() {
        let rt = SandboxRuntime::new().with_max_module_bytes(8);
        let big: Vec<u8> = [0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0xFF].to_vec();
        assert!(matches!(rt.validate(&big), Err(SandboxError::ModuleTooLarge(_, _))));
    }

    #[test]
    fn eval_round_trip_echoes_the_input() {
        let rt = SandboxRuntime::new();
        let result = rt.eval(&echo_wasm(), r#"{"msg":"hello","n":7}"#).unwrap();
        assert!(result.executed);
        assert_eq!(result.output, serde_json::json!({"msg": "hello", "n": 7}));
        assert!(result.fuel_consumed > 0);
        assert_eq!(result.memory_used, 8 * 64 * 1024);
        assert!(result.elapsed_ms < 5_000);
    }

    #[test]
    fn eval_b64_decodes_and_runs() {
        let rt = SandboxRuntime::new();
        let b64 = base64::engine::general_purpose::STANDARD.encode(echo_wasm());
        let result = rt.eval_b64(&b64, r#"{"ok":true}"#).unwrap();
        assert!(result.executed);
        assert_eq!(result.output, serde_json::json!({"ok": true}));
    }

    #[test]
    fn eval_b64_rejects_garbage() {
        let rt = SandboxRuntime::new();
        assert!(matches!(
            rt.eval_b64("!!!not-base64!!!", "{}"),
            Err(SandboxError::Base64Decode(_))
        ));
    }

    #[test]
    fn eval_zero_bytes_is_null_output() {
        let zero_wasm = wat::parse_str(
            r#"(module
            (memory (export "memory") 1)
            (func (export "axiom_eval") (param i32 i32 i32 i32) (result i32)
                (i32.const 0)))"#,
        )
        .unwrap();
        let result = SandboxRuntime::new().eval(&zero_wasm, "{}").unwrap();
        assert_eq!(result.output, serde_json::Value::Null);
    }

    #[test]
    fn eval_reports_module_error_codes() {
        let err_wasm = wat::parse_str(
            r#"(module
            (memory (export "memory") 1)
            (func (export "axiom_eval") (param i32 i32 i32 i32) (result i32)
                (i32.const -1)))"#,
        )
        .unwrap();
        let err = SandboxRuntime::new().eval(&err_wasm, "{}").unwrap_err();
        assert!(matches!(err, SandboxError::ExecutionFailed(msg) if msg.contains("returned -1")));
    }

    #[test]
    fn eval_rejects_imports() {
        let importing = wat::parse_str(
            r#"(module
            (import "env" "host_fn" (func))
            (memory (export "memory") 1)
            (func (export "axiom_eval") (param i32 i32 i32 i32) (result i32)
                (i32.const 0)))"#,
        )
        .unwrap();
        let err = SandboxRuntime::new().eval(&importing, "{}").unwrap_err();
        assert!(matches!(err, SandboxError::ImportRejected(names) if names.contains("env.host_fn")));
    }

    #[test]
    fn eval_fails_without_exports() {
        let bare = wat::parse_str("(module)").unwrap();
        let err = SandboxRuntime::new().eval(&bare, "{}").unwrap_err();
        assert!(matches!(err, SandboxError::ExecutionFailed(msg) if msg.contains("must export 'memory'")));
    }

    #[test]
    fn eval_fails_without_the_eval_export() {
        let no_func = wat::parse_str("(module (memory (export \"memory\") 1))").unwrap();
        let err = SandboxRuntime::new().eval(&no_func, "{}").unwrap_err();
        assert!(matches!(err, SandboxError::ExecutionFailed(msg) if msg.contains("must export 'axiom_eval'")));
    }

    #[test]
    fn eval_dies_by_fuel_on_an_infinite_loop() {
        let looping = wat::parse_str(
            r#"(module
            (memory (export "memory") 1)
            (func (export "axiom_eval") (param i32 i32 i32 i32) (result i32)
                (loop (br 0))
                (i32.const 0)))"#,
        )
        .unwrap();
        let err = SandboxRuntime::new().eval(&looping, "{}").unwrap_err();
        assert!(matches!(err, SandboxError::FuelExhausted(consumed) if consumed > 0));
    }

    #[test]
    fn eval_refuses_input_beyond_memory_with_a_small_module() {
        let rt = SandboxRuntime::new();
        let small = wat::parse_str(
            r#"(module
            (memory (export "memory") 1)
            (func (export "axiom_eval") (param i32 i32 i32 i32) (result i32)
                (i32.const 0)))"#,
        )
        .unwrap();
        let big_input = format!("{{\"pad\":\"{}\"}}", "x".repeat(70 * 1024));
        let err = rt.eval(&small, &big_input).unwrap_err();
        assert!(matches!(
            err,
            SandboxError::MemoryExceeded(min, max) if min == 70 * 1024 + 10 && max == 64 * 1024
        ));
    }

    #[test]
    fn eval_refuses_input_beyond_the_input_region() {
        let rt = SandboxRuntime::new();
        let huge = format!("{{\"pad\":\"{}\"}}", "x".repeat(300 * 1024));
        let err = rt.eval(&echo_wasm(), &huge).unwrap_err();
        assert!(matches!(err, SandboxError::InputTooLarge(len, region) if len == 300 * 1024 + 10 && region == OUTPUT_BASE as usize));
    }

    #[test]
    fn eval_refuses_output_beyond_the_cap() {
        let lying = wat::parse_str(
            r#"(module
            (memory (export "memory") 8)
            (func (export "axiom_eval") (param i32 i32 i32 i32) (result i32)
                (i32.const 1048576)))"#,
        )
        .unwrap();
        let err = SandboxRuntime::new().eval(&lying, "{}").unwrap_err();
        assert!(matches!(err, SandboxError::ExecutionFailed(msg) if msg.contains("output cap")));
    }

    #[test]
    fn eval_times_out_when_fuel_is_huge() {
        // Fuel high enough that the tight loop cannot burn through it
        // inside the 20 ms window (a ~1-5 s budget at realistic burn
        // rates) — the wall-clock timeout fires first, then the join
        // completes when fuel kills the worker. The loop itself never
        // terminates, so without the timeout this test would hang.
        let looping = wat::parse_str(
            r#"(module
            (memory (export "memory") 1)
            (func (export "axiom_eval") (param i32 i32 i32 i32) (result i32)
                (loop (br 0))
                (i32.const 0)))"#,
        )
        .unwrap();
        let rt = SandboxRuntime::new()
            .with_fuel_limit(500_000_000)
            .with_timeout(20);
        let err = rt.eval(&looping, "{}").unwrap_err();
        assert!(matches!(err, SandboxError::ExecutionTimeout(20)));
    }

    #[test]
    fn eval_honors_the_concurrency_cap() {
        let rt = SandboxRuntime::new().with_max_concurrent(0);
        let err = rt.eval(&echo_wasm(), "{}").unwrap_err();
        assert!(matches!(err, SandboxError::ExecutionFailed(msg) if msg.contains("too many concurrent")));
    }
}
