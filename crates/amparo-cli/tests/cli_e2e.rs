//! Real-process e2e for the shipped `amparo` binary.
//!
//! The inference endpoint is a hand-rolled mock LLM bound to a local port
//! (never 11434 — that would route to a real Ollama): `stream: true`
//! requests get scripted `data:` frames, the non-stream verification call
//! gets a fixed `VERIFIED` completion. Env-mutating tests hold [`LOCK`] and
//! restore what they touched — `AMPARO_WORKSPACE` is process-wide state and
//! the mock env must be visible to child processes only while they run.

use amparo_chat::{schedule_dir, JsonScheduleStore, ScheduleStore, ScheduledStatus, ScheduledTask};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn bin() -> String {
    // cargo ≥ 1.89 sets CARGO_BIN_EXE_<name> for integration tests; the
    // pinned MSRV (cargo 1.85) builds the package binaries but never sets
    // the env var, so fall back to the test executable's own location —
    // test binaries live in <profile>/deps/, package bins in <profile>/.
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_amparo") {
        return path;
    }
    let exe = std::env::current_exe().expect("test executable path");
    let profile = exe
        .parent()
        .and_then(|dir| dir.parent())
        .expect("test executable lives in <profile>/deps/");
    let fallback = profile.join(format!("amparo{}", std::env::consts::EXE_SUFFIX));
    assert!(
        fallback.exists(),
        "{} not built — run `cargo build` (or the workspace test gate) first",
        fallback.display()
    );
    fallback.to_string_lossy().into_owned()
}

/// Serializes the tests that mutate environment variables around child
/// spawns.
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// One shared workspace for every spawned binary (created once per test
/// process; the tools create parent dirs themselves where needed).
fn workspace() -> &'static PathBuf {
    static WS: OnceLock<PathBuf> = OnceLock::new();
    WS.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("amparo-cli-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("workspace dir");
        dir
    })
}

fn set_workspace_env() -> Option<String> {
    let prior = std::env::var("AMPARO_WORKSPACE").ok();
    std::env::set_var("AMPARO_WORKSPACE", workspace());
    prior
}

fn restore_workspace_env(prior: Option<String>) {
    match prior {
        Some(v) => std::env::set_var("AMPARO_WORKSPACE", v),
        None => std::env::remove_var("AMPARO_WORKSPACE"),
    }
}

/// Sets (and removes) env vars for the duration of a test; restores
/// everything on drop. Callers hold `LOCK`.
struct EnvGuard(Vec<(String, Option<String>)>);

fn set_env(vars: &[(&str, &str)], removed: &[&str]) -> EnvGuard {
    let mut saved = Vec::new();
    for key in removed {
        saved.push((key.to_string(), std::env::var(key).ok()));
        std::env::remove_var(key);
    }
    for (key, value) in vars {
        saved.push((key.to_string(), std::env::var(key).ok()));
        std::env::set_var(key, value);
    }
    EnvGuard(saved)
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// Hard ceiling for one child run. Linux/macOS finish the slowest test in
/// seconds; Windows runners resolve refused connections to the mock's dead
/// target port (`127.0.0.1:1`) only after TCP SYN retransmits (~2s each),
/// and `fetch_url` retries failed calls up to `MAX_TOOL_RETRIES` times, so
/// the quota test legitimately needs ~60-90s there. The timeout still
/// bounds a real regression hang — it just stops confusing one with
/// platform speed.
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);

/// Spawn `amparo` with a closed stdin and a hard timeout so a regression
/// can never hang the suite. On timeout the child is killed and its
/// partial output rides in the panic — a stall and a slow runner look
/// identical from outside, the bytes tell them apart.
async fn run_with(args: &[&str]) -> Output {
    let mut child = tokio::process::Command::new(bin())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn amparo");
    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    // Collect the pipes in a detached task: it survives the timeout
    // future's drop and completes once the child exits (or is killed).
    let collect = tokio::spawn(async move {
        let (mut so, mut se) = (Vec::new(), Vec::new());
        let (_a, _b) = tokio::join!(
            stdout_pipe.read_to_end(&mut so),
            stderr_pipe.read_to_end(&mut se),
        );
        (so, se)
    });
    match tokio::time::timeout(CHILD_TIMEOUT, child.wait()).await {
        Ok(status) => {
            let status = status.expect("wait amparo");
            let (so, se) = collect.await.expect("collect child output");
            Output {
                status,
                stdout: so,
                stderr: se,
            }
        }
        Err(_) => {
            let _ = child.start_kill();
            let (so, se) = collect.await.expect("collect child output");
            panic!(
                "amparo timed out after 120s: {args:?}\npartial stdout:\n{}\npartial stderr:\n{}",
                String::from_utf8_lossy(&so),
                String::from_utf8_lossy(&se),
            )
        }
    }
}

/// Spawn `amparo` with stdin piped so the test can answer the approval
/// prompt, then wait with the same hard timeout. Dropping the stdin handle
/// sends EOF after the answer.
async fn run_with_stdin(args: &[&str], answer: &[u8]) -> Output {
    let mut child = tokio::process::Command::new(bin())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn amparo");
    if let Some(mut si) = child.stdin.take() {
        si.write_all(answer).await.expect("write to child stdin");
        // Dropping the handle sends EOF after the answer.
    }
    tokio::time::timeout(CHILD_TIMEOUT, child.wait_with_output())
        .await
        .expect("amparo run timed out")
        .expect("wait for amparo")
}

// ── Mock LLM ─────────────────────────────────────────────────────────────────

/// One scripted `stream: true` response: the frames emitted before
/// `data: [DONE]`.
type Script = Vec<Value>;

/// A hand-rolled HTTP server: every connection gets one scripted SSE
/// response (consumed in order; the last script repeats) or the fixed
/// non-stream `VERIFIED` completion. Non-stream request bodies are recorded
/// so tests can inspect what the verification prompt actually carried.
struct MockLlm {
    addr: std::net::SocketAddr,
    /// Every non-stream (`complete`) request body, in arrival order.
    complete_requests: Arc<tokio::sync::Mutex<Vec<Value>>>,
    /// Every `stream: true` request body, in arrival order — lets tests
    /// assert what the model actually saw (tool schemas, tool results).
    stream_requests: Arc<tokio::sync::Mutex<Vec<Value>>>,
}

impl MockLlm {
    async fn start(scripts: Vec<Script>) -> MockLlm {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock");
        let addr = listener.local_addr().unwrap();
        let scripts = Arc::new(tokio::sync::Mutex::new(scripts));
        let complete_requests: Arc<tokio::sync::Mutex<Vec<Value>>> =
            Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&complete_requests);
        let stream_requests: Arc<tokio::sync::Mutex<Vec<Value>>> =
            Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let streamed = Arc::clone(&stream_requests);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let scripts = Arc::clone(&scripts);
                let complete_requests = Arc::clone(&recorded);
                let stream_requests = Arc::clone(&streamed);
                tokio::spawn(async move {
                    // Read the request head, then the body by Content-Length.
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 8192];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                        if buf.len() > 1_000_000 {
                            break;
                        }
                    }
                    let split = buf
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .map(|p| p + 4)
                        .unwrap_or(buf.len());
                    let head = String::from_utf8_lossy(&buf[..split]).to_string();
                    let content_length = head
                        .lines()
                        .find_map(|l| {
                            l.trim_start()
                                .to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    let mut body = buf[split..].to_vec();
                    while body.len() < content_length {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => body.extend_from_slice(&tmp[..n]),
                        }
                    }

                    let streaming = String::from_utf8_lossy(&body).contains("\"stream\":true");
                    if streaming {
                        if let Ok(value) = serde_json::from_slice::<Value>(&body) {
                            stream_requests.lock().await.push(value);
                        }
                    }
                    let response = if streaming {
                        let script = {
                            let mut remaining = scripts.lock().await;
                            if remaining.len() > 1 {
                                remaining.remove(0)
                            } else {
                                remaining.first().cloned().unwrap_or_default()
                            }
                        };
                        let mut out = String::new();
                        for frame in &script {
                            out.push_str(&format!(
                                "data: {}\n\n",
                                serde_json::to_string(frame).unwrap()
                            ));
                        }
                        out.push_str("data: [DONE]\n\n");
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                            out.len(),
                            out
                        )
                    } else {
                        // The verification call: record the prompt so tests
                        // can assert what evidence (if any) it carried.
                        if let Ok(value) = serde_json::from_slice::<Value>(&body) {
                            complete_requests.lock().await.push(value);
                        }
                        let b = json!({
                            "choices": [{
                                "index": 0,
                                "message": {"role": "assistant", "content": "VERIFIED"},
                                "finish_reason": "stop"
                            }],
                            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                        })
                        .to_string();
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                            b.len(),
                            b
                        )
                    };
                    let _ = sock.write_all(response.as_bytes()).await;
                });
            }
        });
        MockLlm {
            addr,
            complete_requests,
            stream_requests,
        }
    }

    /// OpenAI-shaped base URL for `AMPARO_INFERENCE_URL`.
    fn url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    /// Every recorded non-stream (`complete`) request body, in arrival order.
    async fn complete_requests(&self) -> Vec<Value> {
        self.complete_requests.lock().await.clone()
    }

    /// Every recorded `stream: true` request body, in arrival order.
    async fn stream_requests(&self) -> Vec<Value> {
        self.stream_requests.lock().await.clone()
    }

    /// The env surface for the mock endpoint (openai provider is the default).
    fn env(&self) -> Vec<(String, String)> {
        vec![
            ("AMPARO_INFERENCE_URL".to_string(), self.url()),
            (
                "AMPARO_INFERENCE_MODEL".to_string(),
                "mock-model".to_string(),
            ),
        ]
    }
}

// ── Mock policy engine ────────────────────────────────────────────────────────

/// A hand-rolled HTTP responder for the wire policy protocol: every
/// `POST /check` gets one canned verdict. Request bodies are recorded so
/// tests can assert what the wire client actually sent (session ids, tool
/// names).
struct MockPolicy {
    addr: std::net::SocketAddr,
    bodies: Arc<tokio::sync::Mutex<Vec<Value>>>,
}

impl MockPolicy {
    /// Audit-mode responder (M9 W3): `deny` with `enforced: false` —
    /// advisory, never a block.
    async fn start() -> MockPolicy {
        Self::start_with(json!({
            "verdict": "deny",
            "reason": "audit test",
            "enforced": false,
            "engine_verdict": "deny"
        }))
        .await
    }

    /// Escalating responder (M10 W3 e2e): every check escalates to a
    /// human, enforced.
    async fn start_escalating() -> MockPolicy {
        Self::start_with(json!({
            "verdict": "escalate",
            "reason": "new tool, no classification",
            "enforced": true,
            "engine_verdict": "escalate"
        }))
        .await
    }

    /// One canned `verdict` body for every `/check`.
    async fn start_with(verdict: Value) -> MockPolicy {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock policy");
        let addr = listener.local_addr().unwrap();
        let bodies: Arc<tokio::sync::Mutex<Vec<Value>>> =
            Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&bodies);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let recorded = Arc::clone(&recorded);
                let verdict = verdict.clone();
                tokio::spawn(async move {
                    // Read the request head, then the body by Content-Length
                    // (the MockLlm pattern).
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 8192];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                        if buf.len() > 1_000_000 {
                            break;
                        }
                    }
                    let split = buf
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .map(|p| p + 4)
                        .unwrap_or(buf.len());
                    let head = String::from_utf8_lossy(&buf[..split]).to_string();
                    let content_length = head
                        .lines()
                        .find_map(|l| {
                            l.trim_start()
                                .to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    let mut body = buf[split..].to_vec();
                    while body.len() < content_length {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => body.extend_from_slice(&tmp[..n]),
                        }
                    }
                    if let Ok(value) = serde_json::from_slice::<Value>(&body) {
                        recorded.lock().await.push(value);
                    }
                    let b = verdict.to_string();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        b.len(),
                        b
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        MockPolicy { addr, bodies }
    }

    /// Wire-protocol base URL for `--policy-url`.
    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Every recorded `/check` request body, in arrival order.
    async fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().await.clone()
    }
}

/// The web-approval endpoint mock (M10 W4): records POST bodies and
/// serves a canned decision flow — the `poll` closure answers every
/// decision GET (pending until it returns a decided body).
struct MockApprovals {
    addr: std::net::SocketAddr,
    /// Every POSTed approval request, in arrival order.
    bodies: Arc<tokio::sync::Mutex<Vec<Value>>>,
}

impl MockApprovals {
    async fn start(poll: impl Fn() -> Value + Send + Sync + 'static) -> MockApprovals {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock approvals");
        let addr = listener.local_addr().unwrap();
        let bodies: Arc<tokio::sync::Mutex<Vec<Value>>> =
            Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&bodies);
        let poll = Arc::new(poll);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let recorded = Arc::clone(&recorded);
                let poll = Arc::clone(&poll);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 8192];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                        if buf.len() > 1_000_000 {
                            break;
                        }
                    }
                    let split = buf
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .map(|p| p + 4)
                        .unwrap_or(buf.len());
                    let head = String::from_utf8_lossy(&buf[..split]).to_string();
                    let content_length = head
                        .lines()
                        .find_map(|l| {
                            l.trim_start()
                                .to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    let mut body = buf[split..].to_vec();
                    while body.len() < content_length {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => body.extend_from_slice(&tmp[..n]),
                        }
                    }
                    let b = if head.lines().next().unwrap_or_default().starts_with("POST") {
                        if let Ok(value) = serde_json::from_slice::<Value>(&body) {
                            recorded.lock().await.push(value);
                        }
                        json!({"call_id": "call_1", "status": "pending"}).to_string()
                    } else {
                        poll().to_string()
                    };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        b.len(),
                        b
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        MockApprovals { addr, bodies }
    }

    /// Endpoint URL for `--approval-endpoint`.
    fn url(&self) -> String {
        format!("http://{}/approvals", self.addr)
    }

    /// Every POSTed approval request, in arrival order.
    async fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().await.clone()
    }
}

/// A decision poll body: the human answered.
fn web_decided(decision: bool) -> Value {
    json!({"status": "decided", "decision": decision})
}

/// A pending poll body: the human has not answered yet.
fn web_pending() -> Value {
    json!({"status": "pending"})
}

/// One SSE frame carrying a content delta.
fn content_frame(text: &str) -> Value {
    json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": "stop"}]})
}

/// A scripted turn that requests `<tool> <arguments>`: the tool-call frame
/// plus the arguments fragment.
fn tool_script(tool: &str, arguments: &str) -> Script {
    vec![
        json!({
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": tool, "arguments": ""}
                }]},
                "finish_reason": null
            }]
        }),
        json!({
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": [{
                    "index": 0,
                    "function": {"arguments": arguments}
                }]},
                "finish_reason": "tool_calls"
            }]
        }),
    ]
}

/// A scripted turn that requests `run_command <command>`.
fn tool_call_script(command: &str) -> Script {
    tool_script("run_command", &format!("{{\"command\":\"{command}\"}}"))
}

/// A scripted turn that invokes the adopted skill `name`.
fn use_skill_script(name: &str) -> Script {
    tool_script("use_skill", &format!("{{\"skill_name\":\"{name}\"}}"))
}

/// A scripted turn that requests `eval_wasm` with the W1 echo module
/// (built from WAT at test runtime — `wat` and `base64` are dev-only
/// deps, already workspace deps through amparo-sandbox) and `input`.
fn eval_wasm_script(input: &str) -> Script {
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
    let wasm_b64 = base64::engine::general_purpose::STANDARD.encode(wasm);
    let arguments = serde_json::json!({"wasm_base64": wasm_b64, "input": input}).to_string();
    tool_script("eval_wasm", &arguments)
}

/// Sets the mock env and workspace for one test. Callers hold `LOCK`.
async fn mock_env(mock: &MockLlm) -> (EnvGuard, Option<String>) {
    let env_pairs = mock.env();
    let vars: Vec<(&str, &str)> = env_pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let env = set_env(&vars, &[]);
    let prior = set_workspace_env();
    (env, prior)
}

// ── CLI surface ──────────────────────────────────────────────────────────────

#[test]
fn version_prints_name_and_version() {
    let out = std::process::Command::new(bin())
        .arg("version")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        stdout(&out).trim(),
        format!("amparo {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn unknown_subcommand_exits_2() {
    let out = std::process::Command::new(bin())
        .arg("frobnicate")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("unknown subcommand frobnicate"),
        "{}",
        stderr(&out)
    );
}

#[tokio::test]
async fn run_without_env_fails_closed() {
    let _guard = LOCK.lock().await;
    let env = set_env(&[], &["AMPARO_INFERENCE_URL", "AMPARO_INFERENCE_MODEL"]);
    let out = run_with(&["run", "--allow-all", "say hello"]).await;
    drop(env);
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("AMPARO_INFERENCE_URL is required"),
        "fail-closed message names the missing env: {}",
        stderr(&out)
    );
}

#[test]
fn run_unknown_flag_exits_2() {
    let out = std::process::Command::new(bin())
        .args(["run", "--nonsense", "task"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("unknown flag --nonsense"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn run_missing_task_exits_2() {
    let out = std::process::Command::new(bin())
        .arg("run")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("requires a task"), "{}", stderr(&out));
}

#[test]
fn run_approval_conflict_exits_2() {
    let out = std::process::Command::new(bin())
        .args(["run", "--auto-approve", "--auto-deny", "task"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("mutually exclusive"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn run_policy_conflict_exits_2() {
    let out = std::process::Command::new(bin())
        .args([
            "run",
            "--policy-url",
            "http://p.test",
            "--allow-all",
            "task",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("mutually exclusive"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn mcp_serve_subcommand_help_exits_0() {
    let out = std::process::Command::new(bin())
        .args(["mcp-serve", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = stdout(&out);
    assert!(stdout.contains("amparo-mcp-serve"), "{}", stdout);
    assert!(stdout.contains("--trust-ceiling"), "{}", stdout);
}

#[test]
fn mcp_serve_subcommand_rejects_unknown_flag() {
    let out = std::process::Command::new(bin())
        .args(["mcp-serve", "--nonsense"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[tokio::test]
async fn mcp_serve_subcommand_smoke_allow_all() {
    let _guard = LOCK.lock().await;
    let prior = set_workspace_env();
    std::fs::write(workspace().join("marker.txt"), "amparo cli e2e").unwrap();

    let client = amparo_mcp::McpClient::spawn(bin(), ["mcp-serve", "--allow-all"])
        .await
        .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        client.call_tool("list_dir", json!({"path": "."})),
    )
    .await
    .expect("call_tool timed out")
    .unwrap();
    let text = result
        .content
        .iter()
        .find(|c| c.block_type == "text")
        .map(|c| c.text.as_str())
        .unwrap_or_default();
    assert!(
        !result.isError,
        "list_dir should run under --allow-all: {text}"
    );
    assert!(
        text.contains("marker.txt"),
        "the real tool ran against the real workspace: {text}"
    );

    restore_workspace_env(prior);
}

// ── M10 W5: spawn_agent over the MCP surface ─────────────────────────────────

#[tokio::test]
async fn mcp_serve_registers_no_spawn_agent_by_default() {
    let _guard = LOCK.lock().await;
    let prior = set_workspace_env();

    // Without --max-sub-agents the server never builds the inference
    // provider (and so needs no inference env) — spawn_agent stays out
    // of the surface until the operator opts in.
    let client = amparo_mcp::McpClient::spawn(bin(), ["mcp-serve", "--allow-all"])
        .await
        .unwrap();
    assert!(
        client.tools().iter().all(|t| t.name != "spawn_agent"),
        "spawn_agent must not be registered without --max-sub-agents: {:?}",
        client
            .tools()
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>()
    );

    restore_workspace_env(prior);
}

#[tokio::test]
async fn mcp_serve_spawns_agents_within_the_shared_budget() {
    let _guard = LOCK.lock().await;
    let mock = MockLlm::start(vec![vec![content_frame("child done")]]).await;
    let (env, prior) = mock_env(&mock).await;

    let client = amparo_mcp::McpClient::spawn(
        bin(),
        [
            "mcp-serve",
            "--allow-all",
            "--auto-approve",
            "--max-sub-agents",
            "2",
        ],
    )
    .await
    .unwrap();
    assert!(
        client.tools().iter().any(|t| t.name == "spawn_agent"),
        "spawn_agent is registered when the budget is on"
    );

    // Two spawns fit the budget; each child runs the mock loop to its
    // own final answer and reports it back through the tool result.
    for n in 1..=2 {
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            client.call_tool("spawn_agent", json!({"task": "answer the child question"})),
        )
        .await
        .expect("call_tool timed out")
        .unwrap();
        let text = result
            .content
            .iter()
            .find(|c| c.block_type == "text")
            .map(|c| c.text.as_str())
            .unwrap_or_default();
        assert!(!result.isError, "spawn {n} should run: {text}");
        assert!(
            text.contains("child done"),
            "spawn {n} reports the child's final answer head: {text}"
        );
    }

    // The third spawn exhausts the shared budget and fails closed —
    // no child is created.
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        client.call_tool("spawn_agent", json!({"task": "one too many"})),
    )
    .await
    .expect("call_tool timed out")
    .unwrap();
    let text = result
        .content
        .iter()
        .find(|c| c.block_type == "text")
        .map(|c| c.text.as_str())
        .unwrap_or_default();
    assert!(result.isError, "the third spawn must be refused: {text}");
    assert!(
        text.contains("swarm budget exhausted: 2 sub-agents max"),
        "{text}"
    );

    restore_workspace_env(prior);
    drop(env);
}

#[tokio::test]
async fn mcp_serve_denies_a_spawn_without_an_approver() {
    let _guard = LOCK.lock().await;
    // The inference env is only needed to BUILD the spawn tool — no
    // child may run, so the mock never receives a request.
    let mock = MockLlm::start(vec![vec![content_frame("unused")]]).await;
    let (env, prior) = mock_env(&mock).await;

    let client =
        amparo_mcp::McpClient::spawn(bin(), ["mcp-serve", "--allow-all", "--max-sub-agents", "1"])
            .await
            .unwrap();
    assert!(
        client.tools().iter().any(|t| t.name == "spawn_agent"),
        "spawn_agent is registered when the budget is on"
    );

    // spawn_agent is ExternalEffector: without an approver (no
    // --auto-approve, no --approval-endpoint) the gate is AutoDeny and
    // the spawn fails closed — and the server survives the denial.
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        client.call_tool("spawn_agent", json!({"task": "never runs"})),
    )
    .await
    .expect("call_tool timed out")
    .unwrap();
    let text = result
        .content
        .iter()
        .find(|c| c.block_type == "text")
        .map(|c| c.text.as_str())
        .unwrap_or_default();
    assert!(
        result.isError,
        "no approver: the spawn must fail closed: {text}"
    );
    assert!(
        text.contains("User denied the action or approval timed out"),
        "{text}"
    );

    // The server is still serving after the denial.
    assert!(!client.tools().is_empty(), "the server survives the denial");

    restore_workspace_env(prior);
    drop(env);
}

// ── Doctor exit-code matrix ───────────────────────────────────────────────────

#[tokio::test]
async fn doctor_exit_code_matrix() {
    let _guard = LOCK.lock().await;

    // 0: a fresh workspace is healthy — missing files are information.
    let fresh =
        std::env::temp_dir().join(format!("amparo-doctor-e2e-{}-fresh", std::process::id()));
    let _ = std::fs::remove_dir_all(&fresh);
    std::fs::create_dir_all(&fresh).unwrap();
    let out = run_with(&["doctor", "--workspace", fresh.to_str().unwrap()]).await;
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("[doctor] healthy"),
        "the sweep printed its check lines to stdout: {}",
        stdout(&out)
    );

    // 1: a workspace that is a file is a problem.
    let not_dir =
        std::env::temp_dir().join(format!("amparo-doctor-e2e-{}-file", std::process::id()));
    std::fs::write(&not_dir, b"x").unwrap();
    let out = run_with(&["doctor", "--workspace", not_dir.to_str().unwrap()]).await;
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));

    // 1: an unreadable ledger is a problem (unparseable lines count).
    let corrupt =
        std::env::temp_dir().join(format!("amparo-doctor-e2e-{}-corrupt", std::process::id()));
    let _ = std::fs::remove_dir_all(&corrupt);
    std::fs::create_dir_all(corrupt.join(".amparo").join("privacy")).unwrap();
    std::fs::write(
        corrupt.join(".amparo").join("privacy").join("ledger.jsonl"),
        b"not json",
    )
    .unwrap();
    let out = run_with(&["doctor", "--workspace", corrupt.to_str().unwrap()]).await;
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));

    // 2: usage errors — an unknown flag, and a probe without an engine.
    let out = run_with(&["doctor", "--nonsense"]).await;
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    let out = run_with(&["doctor", "--probe"]).await;
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
}

// ── Loop e2e against the mock LLM ────────────────────────────────────────────

#[tokio::test]
async fn run_completes_against_mock_llm() {
    let _guard = LOCK.lock().await;
    let mock = MockLlm::start(vec![vec![content_frame("Hello from the mock.")]]).await;
    let (env, prior) = mock_env(&mock).await;
    let out = run_with(&["run", "--allow-all", "say hello"]).await;
    restore_workspace_env(prior);
    drop(env);

    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "Hello from the mock.");
    assert!(
        stderr(&out).contains("[report] complete"),
        "{}",
        stderr(&out)
    );
}

#[tokio::test]
async fn run_prints_the_power_on_banner_to_stderr() {
    let _guard = LOCK.lock().await;
    let mock = MockLlm::start(vec![vec![content_frame("Hello from the mock.")]]).await;
    let (env, prior) = mock_env(&mock).await;
    let out = run_with(&["run", "--allow-all", "say hello"]).await;
    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(
        err.contains("Greetings! My name is Amparo, built by EL AI Intelligence."),
        "the greeting leads the run: {err}"
    );
    assert!(err.contains("[wake] Amparo is awake."), "{err}");
    assert!(
        err.contains(
            "[gate] chain: registry → trust ceiling (system_control) → policy (allow-all) \
             → human approval (terminal y/N, 60s fail-closed)"
        ),
        "the banner names the wired chain: {err}"
    );
    assert!(
        err.contains("[infer] openai · mock-model · http://127.0.0.1:"),
        "the infer line names the mock provider: {err}"
    );
    assert!(err.contains("[memory] built-in store"), "{err}");
    // stdout purity: the banner is stderr-only.
    assert_eq!(stdout(&out).trim(), "Hello from the mock.");
}

#[tokio::test]
async fn run_executes_approved_tool_end_to_end() {
    let _guard = LOCK.lock().await;
    let marker = format!("amparo-cli-e2e-{}", std::process::id());
    let mock = MockLlm::start(vec![
        tool_call_script(&format!("echo {marker}")),
        vec![content_frame("Done.")],
    ])
    .await;
    let (env, prior) = mock_env(&mock).await;

    let out = run_with_stdin(&["run", "--allow-all", "run the e2e echo"], b"y\n").await;

    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(
        err.contains("[approval] granted"),
        "the human said yes: {err}"
    );
    assert!(
        err.contains("[exec] run_command"),
        "the approved tool ran: {err}"
    );
    assert_eq!(stdout(&out).trim(), "Done.");
}

#[tokio::test]
async fn blackboard_write_emits_a_bus_row_and_the_read_sees_the_value() {
    let _guard = LOCK.lock().await;
    let mock = MockLlm::start(vec![
        tool_script("blackboard_write", r#"{"key":"handoff","value":"42"}"#),
        tool_script("blackboard_read", r#"{"key":"handoff"}"#),
        vec![content_frame("Done.")],
    ])
    .await;
    let (env, prior) = mock_env(&mock).await;

    let out = run_with(&[
        "run",
        "--allow-all",
        "--auto-approve",
        "--session-id",
        "web-1",
        "leave a handoff",
    ])
    .await;

    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(err.contains("[exec] blackboard_write"), "{err}");
    assert!(
        err.contains("[bus] handoff by sess-"),
        "the bus row names the writing task: {err}"
    );
    assert!(err.contains("[exec] blackboard_read"), "{err}");
    assert_eq!(stdout(&out).trim(), "Done.");
}

/// A one-shot HTTP responder (the MockPolicy accept-loop shape) that
/// captures one request and answers 200 — the webhook target for the
/// `send_notification` e2e.
struct MockWebhook {
    addr: std::net::SocketAddr,
    requests: Arc<tokio::sync::Mutex<Vec<String>>>,
}

impl MockWebhook {
    async fn start() -> MockWebhook {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock webhook");
        let addr = listener.local_addr().unwrap();
        let requests: Arc<tokio::sync::Mutex<Vec<String>>> =
            Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let recorded = Arc::clone(&recorded);
                tokio::spawn(async move {
                    // Read the request head, then the body by
                    // Content-Length (the MockPolicy pattern — a read may
                    // overshoot past the delimiter into body bytes, so the
                    // split position is what delimits the head).
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 8192];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    }
                    let split = buf
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .map(|i| i + 4)
                        .unwrap_or(buf.len());
                    let head = String::from_utf8_lossy(&buf[..split]).to_string();
                    let len = head
                        .lines()
                        .find_map(|l| {
                            l.split_once(':').and_then(|(k, v)| {
                                k.trim().eq_ignore_ascii_case("content-length").then_some(v)
                            })
                        })
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    let mut body = buf[split..].to_vec();
                    while body.len() < len {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => body.extend_from_slice(&tmp[..n]),
                        }
                    }
                    recorded
                        .lock()
                        .await
                        .push(format!("{head}{}", String::from_utf8_lossy(&body)));
                    let _ = sock
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                });
            }
        });
        MockWebhook { addr, requests }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    async fn requests(&self) -> Vec<String> {
        self.requests.lock().await.clone()
    }
}

#[tokio::test]
async fn send_notification_without_a_webhook_delivers_to_stderr() {
    let _guard = LOCK.lock().await;
    let mock = MockLlm::start(vec![
        tool_script(
            "send_notification",
            r#"{"destination":"ops-channel","message":"deploy finished"}"#,
        ),
        vec![content_frame("Notified.")],
    ])
    .await;
    let (env, prior) = mock_env(&mock).await;

    let out = run_with(&[
        "run",
        "--allow-all",
        "--auto-approve",
        "notify the ops channel",
    ])
    .await;

    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(err.contains("[exec] send_notification"), "{err}");
    assert!(
        err.contains("[notification] to ops-channel: deploy finished"),
        "the default stderr transport prints the notification: {err}"
    );
    assert_eq!(stdout(&out).trim(), "Notified.");
}

#[tokio::test]
async fn send_notification_posts_to_the_configured_webhook() {
    let _guard = LOCK.lock().await;
    let mock = MockLlm::start(vec![
        tool_script(
            "send_notification",
            r#"{"destination":"ops-channel","message":"deploy finished"}"#,
        ),
        vec![content_frame("Notified.")],
    ])
    .await;
    let webhook = MockWebhook::start().await;
    let (env, prior) = mock_env(&mock).await;

    let out = run_with(&[
        "run",
        "--allow-all",
        "--auto-approve",
        "--webhook-url",
        &webhook.url(),
        "notify the ops channel",
    ])
    .await;

    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(err.contains("[exec] send_notification"), "{err}");
    assert_eq!(stdout(&out).trim(), "Notified.");
    let requests = webhook.requests().await;
    assert_eq!(requests.len(), 1, "one POST: {requests:?}");
    assert!(
        requests[0].contains("POST / HTTP/1.1"),
        "must POST to the webhook URL: {}",
        requests[0]
    );
    assert!(requests[0].contains("ops-channel"), "body: {}", requests[0]);
    assert!(
        requests[0].contains("\"destination\""),
        "body is the notification JSON: {}",
        requests[0]
    );
}

#[tokio::test]
async fn audit_mode_notice_prints_once_and_checks_carry_the_session_id() {
    let _guard = LOCK.lock().await;
    let marker = format!("amparo-cli-e2e-{}", std::process::id());
    // Two tool calls → at least two wire checks, each an audit-only verdict.
    let mock = MockLlm::start(vec![
        tool_call_script(&format!("echo {marker} one")),
        tool_call_script(&format!("echo {marker} two")),
        vec![content_frame("Done.")],
    ])
    .await;
    let policy = MockPolicy::start().await;
    let (env, prior) = mock_env(&mock).await;

    let out = run_with(&[
        "run",
        "--policy-url",
        &policy.url(),
        "--session-id",
        "web-1",
        "--auto-approve",
        "run two tools under an audit-mode engine",
    ])
    .await;

    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert_eq!(stdout(&out).trim(), "Done.");

    // Multiple audit-only verdicts, exactly one notice — process-wide.
    assert_eq!(
        err.matches("policy engine is in audit mode; verdicts are advisory")
            .count(),
        1,
        "the audit notice prints exactly once: {err}"
    );

    // Every wire check carried the explicit session id.
    let bodies = policy.bodies().await;
    assert!(
        bodies.len() >= 2,
        "both tool calls went through the wire engine: {bodies:?}"
    );
    for body in &bodies {
        assert_eq!(
            body["session_id"], "web-1",
            "every check carries the session id: {body}"
        );
    }
}

#[tokio::test]
async fn run_defaults_the_session_id_to_the_task_id() {
    let _guard = LOCK.lock().await;
    let marker = format!("amparo-cli-e2e-{}", std::process::id());
    let mock = MockLlm::start(vec![
        tool_call_script(&format!("echo {marker}")),
        vec![content_frame("Done.")],
    ])
    .await;
    let policy = MockPolicy::start().await;
    let (env, prior) = mock_env(&mock).await;

    let out = run_with(&[
        "run",
        "--policy-url",
        &policy.url(),
        "--auto-approve",
        "run one tool without an explicit session id",
    ])
    .await;

    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert_eq!(stdout(&out).trim(), "Done.");

    // No --session-id → the task id itself tags every check, so the field
    // is never simply absent (that would mean the default was lost).
    let bodies = policy.bodies().await;
    assert!(!bodies.is_empty(), "the tool call was checked: {bodies:?}");
    for body in &bodies {
        let id = body["session_id"].as_str();
        assert!(
            id.is_some_and(|s| !s.is_empty()),
            "the default session id tags every check: {body}"
        );
    }
}

#[tokio::test]
async fn run_approval_prompt_shows_the_blast_radius() {
    let _guard = LOCK.lock().await;
    let marker = format!("amparo-cli-e2e-{}", std::process::id());
    let mock = MockLlm::start(vec![
        tool_call_script(&format!("echo {marker}")),
        vec![content_frame("Done.")],
    ])
    .await;
    let (env, prior) = mock_env(&mock).await;

    let out = run_with_stdin(&["run", "--allow-all", "run the e2e echo"], b"y\n").await;

    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    // run_command is an external effector: the M7 preflight line must
    // name the concrete consequence before the reasons.
    assert!(err.contains("[preflight] blast radius: network"), "{err}");
    assert!(err.contains("[approval] granted"), "{err}");
}

#[tokio::test]
async fn write_over_an_existing_file_emits_the_rollback_row_and_backs_up() {
    let _guard = LOCK.lock().await;
    let ws = workspace();
    let note = ws.join("rollback-note.txt");
    let marker = ws.join("rollback-note.txt.amparo-bak");
    std::fs::remove_file(&marker).ok();
    std::fs::write(&note, "old e2e contents").unwrap();

    let mock = MockLlm::start(vec![
        tool_script(
            "write_file",
            r#"{"path":"rollback-note.txt","content":"new e2e contents"}"#,
        ),
        vec![content_frame("Done.")],
    ])
    .await;
    let (env, prior) = mock_env(&mock).await;

    // Closed stdin: write_file is LocalMutating, so nothing may gate —
    // the run completes and the [rollback] row still fires on stderr.
    let out = run_with(&["run", "--allow-all", "rewrite the note"]).await;

    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert_eq!(stdout(&out).trim(), "Done.");

    // The row names the idempotent undo and the backup marker.
    assert!(
        err.contains("[rollback] restore the previous contents of"),
        "{err}"
    );
    assert!(err.contains("(backup: "), "{err}");
    assert!(
        err.contains("rollback-note.txt.amparo-bak"),
        "the row names the marker: {err}"
    );

    // The write landed and the previous contents were preserved.
    assert_eq!(std::fs::read_to_string(&note).unwrap(), "new e2e contents");
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "old e2e contents"
    );
    let _ = std::fs::remove_file(&note);
    let _ = std::fs::remove_file(&marker);
}

#[tokio::test]
async fn escalated_write_approval_copy_carries_the_rollback_hint() {
    let _guard = LOCK.lock().await;
    let ws = workspace();
    let note = ws.join("rollback-note.txt");
    let marker = ws.join("rollback-note.txt.amparo-bak");
    std::fs::remove_file(&marker).ok();
    std::fs::write(&note, "old e2e contents").unwrap();

    let mock = MockLlm::start(vec![
        tool_script(
            "write_file",
            r#"{"path":"rollback-note.txt","content":"new e2e contents"}"#,
        ),
        vec![content_frame("Done.")],
    ])
    .await;
    let policy = MockPolicy::start_escalating().await;
    let (env, prior) = mock_env(&mock).await;

    // The wire engine escalates the write; the human approves.
    let out = run_with_stdin(
        &["run", "--policy-url", &policy.url(), "rewrite the note"],
        b"y\n",
    )
    .await;

    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert_eq!(stdout(&out).trim(), "Done.");

    // The approval copy shows the undo path with the backup marker,
    // and the approved write executes with its [rollback] row.
    assert!(
        err.contains("[rollback] restore the previous contents of"),
        "the approval copy carries the rollback hint: {err}"
    );
    assert!(
        err.contains("(backup: "),
        "the copy names the backup marker: {err}"
    );
    assert!(err.contains("[approval] granted"), "{err}");
    assert_eq!(std::fs::read_to_string(&note).unwrap(), "new e2e contents");
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "old e2e contents"
    );
    let _ = std::fs::remove_file(&note);
    let _ = std::fs::remove_file(&marker);
}

#[tokio::test]
async fn approval_endpoint_approves_an_escalated_call_and_carries_the_rollback_copy() {
    let _guard = LOCK.lock().await;
    let ws = workspace();
    let note = ws.join("web-note.txt");
    let marker = ws.join("web-note.txt.amparo-bak");
    std::fs::remove_file(&marker).ok();
    std::fs::write(&note, "old web contents").unwrap();

    let mock = MockLlm::start(vec![
        tool_script(
            "write_file",
            r#"{"path":"web-note.txt","content":"new web contents"}"#,
        ),
        vec![content_frame("Done.")],
    ])
    .await;
    let policy = MockPolicy::start_escalating().await;
    let approvals = MockApprovals::start(move || web_decided(true)).await;
    let (env, prior) = mock_env(&mock).await;

    let out = run_with(&[
        "run",
        "--policy-url",
        &policy.url(),
        "--approval-endpoint",
        &approvals.url(),
        "rewrite the note",
    ])
    .await;

    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert_eq!(stdout(&out).trim(), "Done.");
    // The web decision reached the loop through the gate chain.
    assert!(err.contains("[approval] granted"), "{err}");
    assert_eq!(std::fs::read_to_string(&note).unwrap(), "new web contents");
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "old web contents"
    );

    // The endpoint saw the full approval copy: the escalated write with
    // its preflight classification and the rollback hint + marker.
    let bodies = approvals.bodies().await;
    let body = bodies.first().expect("one POST body recorded");
    assert_eq!(body["tool_name"], "write_file");
    assert!(!body["blast_radius"].is_null());
    assert!(body["rollback"]["undo"]
        .as_str()
        .unwrap()
        .contains("restore the previous contents of"));
    assert!(body["rollback"]["markers"][0]
        .as_str()
        .unwrap()
        .contains("web-note.txt.amparo-bak"));
    let _ = std::fs::remove_file(&note);
    let _ = std::fs::remove_file(&marker);
}

#[tokio::test]
async fn approval_endpoint_denies_and_blocks_the_call() {
    let _guard = LOCK.lock().await;
    let ws = workspace();
    let note = ws.join("web-note.txt");
    std::fs::write(&note, "old web contents").unwrap();

    let mock = MockLlm::start(vec![
        tool_script(
            "write_file",
            r#"{"path":"web-note.txt","content":"new web contents"}"#,
        ),
        vec![content_frame("Done.")],
    ])
    .await;
    let policy = MockPolicy::start_escalating().await;
    let approvals = MockApprovals::start(move || web_decided(false)).await;
    let (env, prior) = mock_env(&mock).await;

    let out = run_with(&[
        "run",
        "--policy-url",
        &policy.url(),
        "--approval-endpoint",
        &approvals.url(),
        "rewrite the note",
    ])
    .await;

    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    // The denial is reported to the loop, which carries on (the mock LLM
    // just says "Done.") — the gate itself blocked the call.
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(err.contains("[approval] denied"), "{err}");
    assert!(err.contains("approval_denied"), "{err}");
    assert_eq!(std::fs::read_to_string(&note).unwrap(), "old web contents");
    let _ = std::fs::remove_file(&note);
}

#[tokio::test]
async fn approval_endpoint_polls_until_decided() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let _guard = LOCK.lock().await;
    let ws = workspace();
    let note = ws.join("web-note.txt");
    let marker = ws.join("web-note.txt.amparo-bak");
    std::fs::remove_file(&marker).ok();
    std::fs::write(&note, "old web contents").unwrap();

    let mock = MockLlm::start(vec![
        tool_script(
            "write_file",
            r#"{"path":"web-note.txt","content":"new web contents"}"#,
        ),
        vec![content_frame("Done.")],
    ])
    .await;
    let policy = MockPolicy::start_escalating().await;
    // First poll: pending (the human is still looking at it); every
    // later poll: decided. The gate must keep polling past the first
    // answer.
    let polls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&polls);
    let approvals = MockApprovals::start(move || {
        if counted.fetch_add(1, Ordering::SeqCst) == 0 {
            web_pending()
        } else {
            web_decided(true)
        }
    })
    .await;
    let (env, prior) = mock_env(&mock).await;

    let out = run_with(&[
        "run",
        "--policy-url",
        &policy.url(),
        "--approval-endpoint",
        &approvals.url(),
        "rewrite the note",
    ])
    .await;

    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert_eq!(stdout(&out).trim(), "Done.");
    assert!(
        polls.load(Ordering::SeqCst) >= 2,
        "the gate polled past the pending answer"
    );
    assert_eq!(std::fs::read_to_string(&note).unwrap(), "new web contents");
    let _ = std::fs::remove_file(&note);
    let _ = std::fs::remove_file(&marker);
}

#[tokio::test]
async fn run_auto_denies_on_piped_eof() {
    let _guard = LOCK.lock().await;
    let mock = MockLlm::start(vec![
        tool_call_script("echo never-runs"),
        vec![content_frame("Done.")],
    ])
    .await;
    let (env, prior) = mock_env(&mock).await;

    // stdin is closed — the gate must deny, not hang, and the loop must
    // survive the denial and complete from the next turn.
    let out = run_with(&["run", "--allow-all", "attempt the echo"]).await;

    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(err.contains("no input (EOF) — denying"), "{err}");
    assert!(err.contains("[approval] denied"), "{err}");
    assert_eq!(stdout(&out).trim(), "Done.");
}

#[tokio::test]
async fn run_auto_approve_skips_stdin_entirely() {
    let _guard = LOCK.lock().await;
    let marker = format!("amparo-cli-e2e-{}", std::process::id());
    let mock = MockLlm::start(vec![
        tool_call_script(&format!("echo {marker}")),
        vec![content_frame("Done.")],
    ])
    .await;
    let (env, prior) = mock_env(&mock).await;

    // --auto-approve never constructs the interactive gate: stdin stays
    // closed and no prompt appears.
    let out = run_with(&["run", "--allow-all", "--auto-approve", "run the echo"]).await;

    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(err.contains("[approval] granted"), "{err}");
    assert!(err.contains("[exec] run_command"), "{err}");
    assert!(
        !err.contains("approve? [y/N]"),
        "no prompt may be printed: {err}"
    );
    assert_eq!(stdout(&out).trim(), "Done.");
}

#[tokio::test]
async fn run_spawns_a_child_under_the_shared_gate_and_stamps_both_chains() {
    let _guard = LOCK.lock().await;
    std::fs::remove_dir_all(workspace().join(".amparo")).ok();
    // Five turns on one FIFO queue: the parent fetches (an observational
    // call that never asks a human), spawns a child, the child runs a
    // gated command through the same gate, then both answer.
    let mock = MockLlm::start(vec![
        tool_script("fetch_url", r#"{"url":"http://127.0.0.1:1/parent"}"#),
        tool_script("spawn_agent", r#"{"task":"run the child command"}"#),
        tool_call_script("echo child-side-effect"),
        vec![content_frame("child done")],
        vec![content_frame("Done.")],
    ])
    .await;
    let (env, prior) = mock_env(&mock).await;

    // Two approvals: the spawn itself, then the child's gated command.
    let out = run_with_stdin(&["run", "--allow-all", "delegate the fetch"], b"y\ny\n").await;

    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert_eq!(stdout(&out).trim(), "Done.");

    // The ledger names both chains: the parent row, then the child row
    // chained off it (M8 W4 — the shared sink stamps each row from the
    // frame of the agent whose call executed).
    let ledger = workspace().join(".amparo/privacy/ledger.jsonl");
    let text = std::fs::read_to_string(&ledger).expect("ledger exists");
    let rows: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("one ledger row per line"))
        .collect();
    assert_eq!(
        rows.len(),
        2,
        "one row per network tool, spawn writes none: {text}"
    );
    let parent = rows[0]["task_id"]
        .as_str()
        .expect("parent task id")
        .to_string();
    assert!(parent.starts_with("sess-"), "host-generated id: {text}");
    assert!(
        rows[0].get("parent_task_id").is_none(),
        "a top-level task has no parent: {text}"
    );
    let child = rows[1]["task_id"]
        .as_str()
        .expect("child task id")
        .to_string();
    assert_eq!(
        child,
        format!("{parent}.1"),
        "the child chains off the parent: {text}"
    );
    assert_eq!(
        rows[1]["parent_task_id"].as_str(),
        Some(parent.as_str()),
        "{text}"
    );

    // The approval copy names the sub-agent chain (M8 W2) and the
    // preflight labels the spawn's blast radius (M8 W4).
    assert!(
        err.contains(&format!(
            "[session] sub-agent {child} of task {parent} wants to run:"
        )),
        "{err}"
    );
    assert!(err.contains("[preflight] blast radius: sub_agent"), "{err}");
    assert!(
        err.contains(&format!(
            "[spawn] {child} under {parent}: run the child command"
        )),
        "{err}"
    );

    // The observatory: the swarm line names the child and totals every
    // tool call (the parent's two plus the child's one).
    assert!(
        err.contains(&format!(
            "[swarm] swarm: 1 sub-agent(s) ({child}), 3 tool calls"
        )),
        "{err}"
    );
    assert!(err.contains("[report]"), "{err}");
}

#[tokio::test]
async fn run_executes_approved_eval_wasm_with_read_only_radius() {
    let _guard = LOCK.lock().await;
    let mock = MockLlm::start(vec![
        eval_wasm_script(r#"{"msg":"hello"}"#),
        vec![content_frame("Done.")],
    ])
    .await;
    let (env, prior) = mock_env(&mock).await;
    std::fs::remove_dir_all(workspace().join(".amparo")).ok();

    let out = run_with_stdin(&["run", "--allow-all", "evaluate the module"], b"y\n").await;

    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    // M7b: the sandbox computes in place — the preflight radius is
    // read_only even though the tier is an external effector.
    assert!(err.contains("[preflight] blast radius: read_only"), "{err}");
    assert!(err.contains("[approval] granted"), "{err}");
    assert!(
        err.contains("[exec] eval_wasm"),
        "the approved module ran: {err}"
    );
    assert_eq!(stdout(&out).trim(), "Done.");

    // The sandbox contract reaches the model: the first request carries
    // the eval_wasm schema with its stated limits, and the second —
    // after execution — carries the module's output. The result feeds
    // the loop.
    let streams = mock.stream_requests().await;
    assert!(
        streams.len() >= 2,
        "tool call, then post-execution turn: {streams:?}"
    );
    assert!(
        streams[0].to_string().contains("10M fuel"),
        "the schema states the limits: {}",
        streams[0]
    );
    assert!(
        streams[1].to_string().contains("hello"),
        "the echo output feeds the loop: {}",
        streams[1]
    );

    // Nothing left the machine: the always-on ledger (its file exists
    // because the sink opens at task start) holds no eval_wasm row.
    let ledger = workspace().join(".amparo/privacy/ledger.jsonl");
    let text = std::fs::read_to_string(&ledger).unwrap_or_default();
    assert!(
        !text.contains("eval_wasm"),
        "eval_wasm never leaves the machine: {text}"
    );
}

#[tokio::test]
async fn run_denied_eval_wasm_executes_nothing_and_writes_no_row() {
    let _guard = LOCK.lock().await;
    let mock = MockLlm::start(vec![
        eval_wasm_script(r#"{"msg":"never"}"#),
        vec![content_frame("Done.")],
    ])
    .await;
    let (env, prior) = mock_env(&mock).await;
    std::fs::remove_dir_all(workspace().join(".amparo")).ok();

    // stdin is closed — the gate denies the untrusted module and the
    // loop survives to the next turn.
    let out = run_with(&["run", "--allow-all", "evaluate the module"]).await;

    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(err.contains("[preflight] blast radius: read_only"), "{err}");
    assert!(err.contains("no input (EOF) — denying"), "{err}");
    assert!(err.contains("[approval] denied"), "{err}");
    assert!(
        !err.contains("[exec] eval_wasm"),
        "a denied module never runs: {err}"
    );
    assert_eq!(stdout(&out).trim(), "Done.");

    // And unlike a denied network tool, the denial writes no ledger row:
    // eval_wasm is not a network tool, so the ledger holds nothing.
    let ledger = workspace().join(".amparo/privacy/ledger.jsonl");
    let text = std::fs::read_to_string(&ledger).unwrap_or_default();
    assert!(!text.contains("eval_wasm"), "{text}");
}

#[tokio::test]
async fn run_growth_writes_a_pii_stripped_run_record() {
    let _guard = LOCK.lock().await;
    let marker = format!("amparo-cli-e2e-{}", std::process::id());
    let mock = MockLlm::start(vec![
        tool_call_script(&format!("echo {marker}")),
        vec![content_frame("Done.")],
    ])
    .await;
    let (env, prior) = mock_env(&mock).await;
    // A previous growth run in this test process may have left records.
    std::fs::remove_dir_all(workspace().join(".amparo")).ok();

    // The task carries an email address: the record must hold only the
    // placeholder, never the raw address.
    let task = format!("please email user@example.com about {marker}");
    let out = run_with(&["run", "--allow-all", "--auto-approve", "--growth", &task]).await;

    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(
        err.contains("[growth] recording PII-stripped run records"),
        "{err}"
    );

    let records = workspace().join(".amparo/notebook/records.jsonl");
    let text = std::fs::read_to_string(&records).expect("records file exists");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "exactly one record: {text}");
    assert!(
        !text.contains("user@example.com"),
        "raw email persisted: {text}"
    );

    // One MemoryEntry per line; the entry's content is the RunRecord JSON.
    let entry: Value = serde_json::from_str(lines[0]).expect("line is JSON");
    let record: Value =
        serde_json::from_str(entry["content"].as_str().expect("content")).expect("record JSON");
    assert_eq!(record["tenant_id"], "cli");
    assert!(
        record["task_text"]
            .as_str()
            .unwrap_or_default()
            .contains("[EMAIL_1]"),
        "task text carries the placeholder: {record}"
    );
    let call = &record["tool_calls"][0];
    assert_eq!(call["tool_name"], "run_command");
    assert_eq!(call["decision"], "allowed");
    assert_eq!(call["approved"], true);
    assert_eq!(record["status"], "complete");
}

#[tokio::test]
async fn run_without_growth_creates_no_records_file() {
    let _guard = LOCK.lock().await;
    let mock = MockLlm::start(vec![
        tool_call_script("echo no-growth"),
        vec![content_frame("Done.")],
    ])
    .await;
    let (env, prior) = mock_env(&mock).await;
    std::fs::remove_dir_all(workspace().join(".amparo")).ok();

    let out = run_with(&["run", "--allow-all", "--auto-approve", "no growth here"]).await;

    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(
        !err.contains("[growth]"),
        "no growth line without the flag: {err}"
    );
    assert!(
        !workspace().join(".amparo/notebook/records.jsonl").exists(),
        "no flag means no records file"
    );
}

#[tokio::test]
async fn run_growth_retrieves_prior_cases_into_verification() {
    let _guard = LOCK.lock().await;
    let mock = MockLlm::start(vec![vec![content_frame("Deployed.")]]).await;
    let (env, prior) = mock_env(&mock).await;
    std::fs::remove_dir_all(workspace().join(".amparo")).ok();

    // Run A seeds the notebook with a VERIFIED case; run B's verification
    // prompt must carry it as read-only evidence (M6b).
    let task = "deploy the staging site";
    let out_a = run_with(&["run", "--allow-all", "--auto-approve", "--growth", task]).await;
    let err_a = stderr(&out_a);
    assert_eq!(out_a.status.code(), Some(0), "stderr: {err_a}");
    let out_b = run_with(&["run", "--allow-all", "--auto-approve", "--growth", task]).await;
    let err_b = stderr(&out_b);
    assert_eq!(out_b.status.code(), Some(0), "stderr: {err_b}");

    restore_workspace_env(prior);
    drop(env);

    let requests = mock.complete_requests().await;
    assert_eq!(
        requests.len(),
        2,
        "one verification call per run: {requests:?}"
    );
    let prompt = |request: &Value| {
        request["messages"][0]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    assert!(
        !prompt(&requests[0]).contains("Prior cases"),
        "an empty notebook must leave the verification prompt unchanged"
    );
    let prompt_b = prompt(&requests[1]);
    assert!(
        prompt_b.contains("Prior cases in this tenant resembling the current task:"),
        "the second run's verification prompt carries the evidence section: {prompt_b}"
    );
    assert!(
        prompt_b.contains("deploy the staging site"),
        "the evidence names the prior task: {prompt_b}"
    );
}

#[tokio::test]
async fn run_without_growth_has_no_evidence_in_verification() {
    let _guard = LOCK.lock().await;
    let mock = MockLlm::start(vec![vec![content_frame("Done.")]]).await;
    let (env, prior) = mock_env(&mock).await;
    std::fs::remove_dir_all(workspace().join(".amparo")).ok();

    let out = run_with(&["run", "--allow-all", "--auto-approve", "plain run"]).await;

    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    let requests = mock.complete_requests().await;
    assert!(!requests.is_empty(), "the verification call still happens");
    let prompt = requests[0]["messages"][0]["content"]
        .as_str()
        .unwrap_or_default();
    assert!(
        !prompt.contains("Prior cases"),
        "without --growth the verification prompt must stay unchanged: {prompt}"
    );
}

// ── Skills (M6c) ─────────────────────────────────────────────────────────────

/// The e2e candidate: two workspace steps, adoptable behind the gates.
const SKILL_CANDIDATE: &str = "name = \"e2e-greet\"\n\
description = \"writes the e2e skill marker and reads it back\"\n\
preconditions = []\n\
origin = \"operator\"\n\
expected_outcome = \"marker file written and read back\"\n\
[[steps]]\n\
tool = \"write_file\"\n\
arguments = { path = \"skill-marker.txt\", content = \"from-skill\" }\n\
[[steps]]\n\
tool = \"read_file\"\n\
arguments = { path = \"skill-marker.txt\" }\n";

/// Seed the candidate (add) and adopt it under the given adopt flags.
/// Callers hold `LOCK` and have set `AMPARO_WORKSPACE`.
async fn seed_skill(adopt_args: &[&str]) -> (Output, Output) {
    std::fs::remove_dir_all(workspace().join(".amparo")).ok();
    let candidate = workspace().join("e2e-greet.toml");
    std::fs::write(&candidate, SKILL_CANDIDATE).unwrap();
    let added = run_with(&["skill", "add", candidate.to_str().unwrap()]).await;
    let mut args = vec!["skill", "adopt", "e2e-greet"];
    args.extend_from_slice(adopt_args);
    let adopted = run_with(&args).await;
    (added, adopted)
}

#[tokio::test]
async fn skill_surface_usage_errors_exit_2() {
    let _guard = LOCK.lock().await;
    let out = run_with(&["skill"]).await;
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("missing subcommand"),
        "{}",
        stderr(&out)
    );
    let out = run_with(&["skill", "frobnicate"]).await;
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    let out = run_with(&["skill", "add"]).await;
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("exactly one candidate file"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn skill_help_exits_0() {
    let out = std::process::Command::new(bin())
        .arg("skill")
        .arg("--help")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = stdout(&out);
    assert!(stdout.contains("amparo skill"), "{}", stdout);
    assert!(stdout.contains("adopt"), "{}", stdout);
}

#[tokio::test]
async fn skill_add_validates_and_writes_the_candidate() {
    let _guard = LOCK.lock().await;
    let prior = set_workspace_env();
    std::fs::remove_dir_all(workspace().join(".amparo")).ok();

    // A spec whose step calls an unknown tool must be refused, nothing written.
    let bad = workspace().join("bad-skill.toml");
    std::fs::write(&bad, SKILL_CANDIDATE.replace("write_file", "no_such_tool")).unwrap();
    let out = run_with(&["skill", "add", bad.to_str().unwrap()]).await;
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("no_such_tool"), "{}", stderr(&out));
    assert!(!workspace()
        .join(".amparo/skills/candidates/bad-skill.toml")
        .exists());

    // The valid candidate lands in candidates/ — not adopted.
    let candidate = workspace().join("e2e-greet.toml");
    std::fs::write(&candidate, SKILL_CANDIDATE).unwrap();
    let out = run_with(&["skill", "add", candidate.to_str().unwrap()]).await;
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(err.contains("[skills] candidate"), "{err}");
    let written = workspace().join(".amparo/skills/candidates/e2e-greet.toml");
    assert!(written.exists(), "candidate file must exist");
    assert!(
        !workspace().join(".amparo/skills/adopted.jsonl").exists(),
        "add never adopts"
    );
    restore_workspace_env(prior);
}

#[tokio::test]
async fn skill_adopt_deny_all_refuses_without_policy() {
    let _guard = LOCK.lock().await;
    let prior = set_workspace_env();
    let (added, adopted) = seed_skill(&[]).await;
    assert_eq!(added.status.code(), Some(0), "stderr: {}", stderr(&added));
    let err = stderr(&adopted);
    assert_eq!(adopted.status.code(), Some(1), "stderr: {err}");
    assert!(
        err.contains("no policy configured"),
        "deny-all reason surfaces: {err}"
    );
    assert!(!workspace().join(".amparo/skills/adopted.jsonl").exists());
    restore_workspace_env(prior);
}

#[tokio::test]
async fn skill_adopt_allow_all_auto_approve_writes_the_audit_log() {
    let _guard = LOCK.lock().await;
    let prior = set_workspace_env();
    let (_, adopted) = seed_skill(&["--allow-all", "--auto-approve"]).await;
    let err = stderr(&adopted);
    assert_eq!(adopted.status.code(), Some(0), "stderr: {err}");
    assert!(
        err.contains("[skills] adopted e2e-greet for tenant cli"),
        "{err}"
    );
    let log = workspace().join(".amparo/skills/adopted.jsonl");
    let text = std::fs::read_to_string(&log).expect("audit log exists");
    let record: Value =
        serde_json::from_str(text.lines().next().expect("one line")).expect("line is JSON");
    assert_eq!(record["event"], "adopt");
    assert_eq!(record["tenant_id"], "cli");
    assert_eq!(record["name"], "e2e-greet");
    assert_eq!(record["spec"]["steps"][0]["tool"], "write_file");
    restore_workspace_env(prior);
}

#[tokio::test]
async fn skill_adopt_auto_deny_writes_nothing() {
    let _guard = LOCK.lock().await;
    let prior = set_workspace_env();
    let (_, adopted) = seed_skill(&["--allow-all", "--auto-deny"]).await;
    let err = stderr(&adopted);
    assert_eq!(adopted.status.code(), Some(1), "stderr: {err}");
    assert!(err.contains("not approved"), "{err}");
    assert!(!workspace().join(".amparo/skills/adopted.jsonl").exists());
    restore_workspace_env(prior);
}

#[tokio::test]
async fn skill_list_and_show_render_the_adopted_skill() {
    let _guard = LOCK.lock().await;
    let prior = set_workspace_env();
    let (_, adopted) = seed_skill(&["--allow-all", "--auto-approve"]).await;
    assert_eq!(
        adopted.status.code(),
        Some(0),
        "stderr: {}",
        stderr(&adopted)
    );

    let out = run_with(&["skill", "list"]).await;
    let out_stdout = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(
        out_stdout.contains("e2e-greet — writes the e2e skill marker"),
        "{out_stdout}"
    );

    let out = run_with(&["skill", "show", "e2e-greet"]).await;
    let out_stdout = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(out_stdout.contains("step 1: write_file"), "{out_stdout}");
    assert!(
        out_stdout.contains("step 2: read_file {\"path\":\"skill-marker.txt\"}"),
        "{out_stdout}"
    );
    assert!(out_stdout.contains("origin: operator"), "{out_stdout}");

    // An unknown skill is a runtime failure, not a usage error.
    let out = run_with(&["skill", "show", "no-such-skill"]).await;
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    restore_workspace_env(prior);
}

#[tokio::test]
async fn run_growth_executes_an_adopted_skill_with_step_records() {
    let _guard = LOCK.lock().await;
    let prior = set_workspace_env();
    let (_, adopted) = seed_skill(&["--allow-all", "--auto-approve"]).await;
    assert_eq!(
        adopted.status.code(),
        Some(0),
        "stderr: {}",
        stderr(&adopted)
    );

    let mock = MockLlm::start(vec![
        use_skill_script("e2e-greet"),
        vec![content_frame("Skill done.")],
    ])
    .await;
    let (env, _prior2) = mock_env(&mock).await;

    let out = run_with(&["run", "--allow-all", "--growth", "use the e2e skill"]).await;

    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(
        err.contains("[growth] skills: 1 adopted for tenant cli"),
        "{err}"
    );
    assert!(
        workspace().join("skill-marker.txt").exists(),
        "the skill's write_file step ran in the workspace"
    );
    assert_eq!(stdout(&out).trim(), "Skill done.");

    // The run record carries the use_skill call AND both expansion steps.
    let text = std::fs::read_to_string(workspace().join(".amparo/notebook/records.jsonl"))
        .expect("records exist");
    let record: Value = serde_json::from_str(
        serde_json::from_str::<Value>(text.lines().next().unwrap()).unwrap()["content"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let names: Vec<&str> = record["tool_calls"]
        .as_array()
        .expect("tool_calls array")
        .iter()
        .map(|call| call["tool_name"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        names,
        vec!["use_skill", "write_file", "read_file"],
        "one use_skill record plus one per step: {names:?}"
    );
    let ids: Vec<&str> = record["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|call| call["call_id"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        ids,
        vec!["call_1", "call_1-step-0", "call_1-step-1"],
        "synthetic step ids are recorded: {ids:?}"
    );
    let steps_succeeded = record["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .all(|call| call["success"] == true);
    assert!(steps_succeeded, "every step succeeded: {record}");
}

#[tokio::test]
async fn run_without_growth_never_registers_use_skill() {
    let _guard = LOCK.lock().await;
    let prior = set_workspace_env();
    // The growth e2e may have left this marker in the shared workspace —
    // this test proves no step runs, so the marker must not pre-exist.
    std::fs::remove_file(workspace().join("skill-marker.txt")).ok();
    let (_, adopted) = seed_skill(&["--allow-all", "--auto-approve"]).await;
    assert_eq!(
        adopted.status.code(),
        Some(0),
        "stderr: {}",
        stderr(&adopted)
    );

    let mock = MockLlm::start(vec![
        use_skill_script("e2e-greet"),
        vec![content_frame("Recovered.")],
    ])
    .await;
    let (env, _prior2) = mock_env(&mock).await;

    let out = run_with(&["run", "--allow-all", "use the e2e skill"]).await;

    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(
        err.contains("[gate] use_skill: unknown_tool"),
        "without --growth the tool is not registered: {err}"
    );
    assert!(
        !workspace().join("skill-marker.txt").exists(),
        "no step may run without growth"
    );
    assert!(
        !workspace().join(".amparo/notebook/records.jsonl").exists(),
        "no growth means no records"
    );
}

#[tokio::test]
async fn skill_propose_distills_recurring_verified_sequences() {
    let _guard = LOCK.lock().await;
    std::fs::remove_dir_all(workspace().join(".amparo")).ok();

    // Three VERIFIED runs of the same single-tool sequence — a fresh mock
    // per run, because the script queue is consumed by the first run.
    for _ in 0..3 {
        let mock = MockLlm::start(vec![
            tool_script("list_dir", "{}"),
            vec![content_frame("Listed.")],
        ])
        .await;
        let (env, prior) = mock_env(&mock).await;
        let out = run_with(&[
            "run",
            "--allow-all",
            "--auto-approve",
            "--growth",
            "list the workspace",
        ])
        .await;
        restore_workspace_env(prior);
        drop(env);
        assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    }

    let prior = set_workspace_env();
    let out = run_with(&["skill", "propose"]).await;
    let err = stderr(&out);
    let out_stdout = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(err.contains("[skills] 1 proposal(s)"), "{err}");
    assert!(
        out_stdout.contains("list-dir"),
        "suggested name: {out_stdout}"
    );
    assert!(
        out_stdout.contains("origin = \"distilled\""),
        "{out_stdout}"
    );
    assert!(
        out_stdout.contains("arguments = {}"),
        "inert skeleton: {out_stdout}"
    );
    assert!(
        out_stdout.contains("3 run(s), 3 VERIFIED"),
        "evidence: {out_stdout}"
    );
    let proposals = workspace().join(".amparo/skills/proposals.jsonl");
    assert_eq!(
        std::fs::read_to_string(&proposals).unwrap().lines().count(),
        1,
        "one proposal logged"
    );

    // Re-running dedupes: the same sequence is not proposed twice.
    let out = run_with(&["skill", "propose"]).await;
    assert!(
        stderr(&out).contains("[skills] 0 proposal(s)"),
        "{}",
        stderr(&out)
    );

    restore_workspace_env(prior);
}

// ── Skills (M6d): metrics and retirement ─────────────────────────────────────

/// One `use_skill` call in a hand-seeded record.
fn seed_use(call_id: &str, target: &str, decision: &str) -> Value {
    json!({
        "call_id": call_id,
        "tool_name": "use_skill",
        "target": target,
        "decision": decision,
        "reasons": [],
        "escalated": false,
        "approved": null,
        "success": true,
        "summary": null,
        "duration_ms": 1
    })
}

/// One expanded skill step attributed to the use call.
fn seed_step(call_id: &str, tool: &str) -> Value {
    json!({
        "call_id": call_id,
        "tool_name": tool,
        "target": "",
        "decision": "allowed",
        "reasons": [],
        "escalated": false,
        "approved": null,
        "success": true,
        "summary": null,
        "duration_ms": 1
    })
}

/// Hand-write `records.jsonl` (MemoryEntry wrappers, the `read_uses`
/// double-parse shape): three `e2e-greet` uses — one VERIFIED, two not —
/// one with the legacy compact-JSON target. Callers hold `LOCK` and have
/// adopted the skill.
fn seed_use_records() {
    let record =
        |started_at: &str, calls: Vec<Value>, status: &str, verification: Option<&str>| -> Value {
            json!({
                "version": 1,
                "tenant_id": "cli",
                "started_at": started_at,
                "duration_ms": 10,
                "task_text": "use the e2e skill",
                "tool_sequence_hash": "seeded",
                "tool_calls": calls,
                "verification": verification.map(|d| json!({"decision": d, "feedback": null})),
                "status": status,
                "final_answer": null,
                "token_cost_estimate": 1
            })
        };
    let rows = [
        record(
            "2026-08-29T00:00:01Z",
            vec![
                seed_use("call_1", "e2e-greet", "allowed"),
                seed_step("call_1-step-0", "write_file"),
                seed_step("call_1-step-1", "read_file"),
            ],
            "complete",
            Some("complete"),
        ),
        // The legacy compact-JSON target is still attributed to the skill.
        record(
            "2026-08-29T00:00:02Z",
            vec![
                seed_use(
                    "call_2",
                    &json!({"skill_name": "e2e-greet"}).to_string(),
                    "allowed",
                ),
                seed_step("call_2-step-0", "write_file"),
            ],
            "complete",
            Some("incomplete"),
        ),
        record(
            "2026-08-29T00:00:03Z",
            vec![seed_use("call_3", "e2e-greet", "allowed")],
            "failed",
            None,
        ),
    ];
    let path = workspace().join(".amparo/notebook/records.jsonl");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut text = String::new();
    for row in rows {
        text.push_str(
            &serde_json::to_string(&json!({"content": row.to_string()})).expect("wrap record"),
        );
        text.push('\n');
    }
    std::fs::write(path, text).expect("write records");
}

#[tokio::test]
async fn skill_retire_refuses_unknown() {
    let _guard = LOCK.lock().await;
    let prior = set_workspace_env();
    let (_, adopted) = seed_skill(&["--allow-all", "--auto-approve"]).await;
    assert_eq!(
        adopted.status.code(),
        Some(0),
        "stderr: {}",
        stderr(&adopted)
    );

    let out = run_with(&["skill", "retire", "no-such-skill"]).await;
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("is not currently adopted"),
        "{}",
        stderr(&out)
    );
    restore_workspace_env(prior);
}

#[tokio::test]
async fn skill_retire_writes_event_list_hides_show_keeps_history() {
    let _guard = LOCK.lock().await;
    let prior = set_workspace_env();
    let (_, adopted) = seed_skill(&["--allow-all", "--auto-approve"]).await;
    assert_eq!(
        adopted.status.code(),
        Some(0),
        "stderr: {}",
        stderr(&adopted)
    );

    let out = run_with(&["skill", "retire", "e2e-greet"]).await;
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(
        err.contains("[skills] retired e2e-greet for tenant cli: operator retired"),
        "{err}"
    );

    // The audit log keeps both events; the last one retires.
    let log = workspace().join(".amparo/skills/adopted.jsonl");
    let text = std::fs::read_to_string(&log).expect("audit log exists");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "adopt + retire: {text}");
    let row: Value = serde_json::from_str(lines[1]).expect("retire row is JSON");
    assert_eq!(row["event"], "retire");
    assert_eq!(row["name"], "e2e-greet");
    assert_eq!(row["reason"], "operator retired");

    // list hides the retired skill…
    let out = run_with(&["skill", "list"]).await;
    let out_stdout = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(
        !out_stdout.contains("e2e-greet"),
        "retired skills leave the list: {out_stdout}"
    );

    // …but show keeps the full history (retirement never deletes).
    let out = run_with(&["skill", "show", "e2e-greet"]).await;
    let out_stdout = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(
        out_stdout.contains("status: retired (operator retired,"),
        "{out_stdout}"
    );
    assert!(out_stdout.contains("retirement history:"), "{out_stdout}");
    assert!(out_stdout.contains("operator retired"), "{out_stdout}");
    restore_workspace_env(prior);
}

#[tokio::test]
async fn skill_check_dry_run_writes_nothing() {
    let _guard = LOCK.lock().await;
    let prior = set_workspace_env();
    let (_, adopted) = seed_skill(&["--allow-all", "--auto-approve"]).await;
    assert_eq!(
        adopted.status.code(),
        Some(0),
        "stderr: {}",
        stderr(&adopted)
    );

    // No policy flags → DenyAll: drift fires, but --dry-run reports only.
    let out = run_with(&["skill", "check", "--dry-run"]).await;
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(err.contains("retired e2e-greet for tenant cli"), "{err}");
    assert!(err.contains("(dry-run — nothing written)"), "{err}");

    let log = workspace().join(".amparo/skills/adopted.jsonl");
    assert_eq!(
        std::fs::read_to_string(&log).unwrap().lines().count(),
        1,
        "no retire event was written"
    );
    assert!(
        !workspace().join(".amparo/skills/rechecks.jsonl").exists(),
        "no recheck row was written"
    );
    restore_workspace_env(prior);
}

#[tokio::test]
async fn skill_check_retires_on_performance() {
    let _guard = LOCK.lock().await;
    let prior = set_workspace_env();
    let (_, adopted) = seed_skill(&["--allow-all", "--auto-approve"]).await;
    assert_eq!(
        adopted.status.code(),
        Some(0),
        "stderr: {}",
        stderr(&adopted)
    );
    seed_use_records();

    // 1 VERIFIED of 3 uses → 33% < 0.5, over the window with the 3-use
    // floor met → retires.
    let out = run_with(&["skill", "check", "--allow-all"]).await;
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(
        err.contains(
            "[skills] retired e2e-greet for tenant cli: performance: VERIFIED rate 33% (1/3) over the last 3 uses"
        ),
        "{err}"
    );
    assert!(
        err.contains("[skills] checked 1 skill(s) for tenant cli, 1 retired"),
        "{err}"
    );

    // A Retired recheck row and a retire event both landed.
    let rechecks = std::fs::read_to_string(workspace().join(".amparo/skills/rechecks.jsonl"))
        .expect("recheck row written");
    let row: Value = serde_json::from_str(rechecks.lines().next().unwrap()).unwrap();
    assert_eq!(row["kind"], "performance");
    assert_eq!(row["outcome"], "retired");
    let log = std::fs::read_to_string(workspace().join(".amparo/skills/adopted.jsonl")).unwrap();
    let row: Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
    assert_eq!(row["event"], "retire");

    // list hides; show carries the metrics and re-check history.
    let out = run_with(&["skill", "list"]).await;
    assert!(!stdout(&out).contains("e2e-greet"), "{}", stdout(&out));
    let out = run_with(&["skill", "show", "e2e-greet"]).await;
    let out_stdout = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(out_stdout.contains("uses: 3"), "{out_stdout}");
    assert!(
        out_stdout.contains("VERIFIED rate: 33% (1/3)"),
        "{out_stdout}"
    );
    assert!(out_stdout.contains("mean steps: 1.00"), "{out_stdout}");
    assert!(
        out_stdout.contains("last policy re-check: "),
        "{out_stdout}"
    );
    assert!(
        out_stdout.contains("(performance, retired)"),
        "{out_stdout}"
    );
    assert!(out_stdout.contains("retirement history:"), "{out_stdout}");
    restore_workspace_env(prior);
}

#[tokio::test]
async fn skill_check_allow_all_with_no_records_is_ok() {
    let _guard = LOCK.lock().await;
    let prior = set_workspace_env();
    let (_, adopted) = seed_skill(&["--allow-all", "--auto-approve"]).await;
    assert_eq!(
        adopted.status.code(),
        Some(0),
        "stderr: {}",
        stderr(&adopted)
    );

    // No records → no uses → below the 3-use floor: nothing retires.
    let out = run_with(&["skill", "check", "--allow-all"]).await;
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(
        err.contains("[skills] e2e-greet: ok — no policy drift, performance within the threshold"),
        "{err}"
    );
    assert!(
        err.contains("[skills] checked 1 skill(s) for tenant cli, 0 retired"),
        "{err}"
    );

    let rechecks = std::fs::read_to_string(workspace().join(".amparo/skills/rechecks.jsonl"))
        .expect("recheck row written");
    let row: Value = serde_json::from_str(rechecks.lines().next().unwrap()).unwrap();
    assert_eq!(row["kind"], "performance");
    assert_eq!(row["outcome"], "ok");
    // The adoption stands: the log still ends with the adopt event.
    let log = std::fs::read_to_string(workspace().join(".amparo/skills/adopted.jsonl")).unwrap();
    let row: Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
    assert_eq!(row["event"], "adopt");
    restore_workspace_env(prior);
}

#[tokio::test]
async fn skill_check_drift_retires_under_deny_all() {
    let _guard = LOCK.lock().await;
    let prior = set_workspace_env();
    let (_, adopted) = seed_skill(&["--allow-all", "--auto-approve"]).await;
    assert_eq!(
        adopted.status.code(),
        Some(0),
        "stderr: {}",
        stderr(&adopted)
    );

    // No policy flags → DenyAll: the step plan would no longer pass.
    let out = run_with(&["skill", "check"]).await;
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(
        err.contains(
            "[skills] retired e2e-greet for tenant cli: policy drift: step 1 (write_file) would be blocked"
        ),
        "{err}"
    );
    assert!(
        err.contains("no policy configured"),
        "the reason names the config gap: {err}"
    );

    let log = std::fs::read_to_string(workspace().join(".amparo/skills/adopted.jsonl")).unwrap();
    let row: Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
    assert_eq!(row["event"], "retire");
    let rechecks = std::fs::read_to_string(workspace().join(".amparo/skills/rechecks.jsonl"))
        .expect("recheck row written");
    let row: Value = serde_json::from_str(rechecks.lines().next().unwrap()).unwrap();
    assert_eq!(row["kind"], "drift");
    assert_eq!(row["outcome"], "retired");
    restore_workspace_env(prior);
}

#[tokio::test]
async fn run_growth_retires_a_drifted_skill_before_registration() {
    let _guard = LOCK.lock().await;
    let prior = set_workspace_env();
    let (_, adopted) = seed_skill(&["--allow-all", "--auto-approve"]).await;
    assert_eq!(
        adopted.status.code(),
        Some(0),
        "stderr: {}",
        stderr(&adopted)
    );

    // The task runs without --allow-all: DenyAll policy → the startup
    // drift check retires the skill before use_skill is ever registered.
    let mock = MockLlm::start(vec![vec![content_frame("Nothing to do.")]]).await;
    let (env, _p1) = mock_env(&mock).await;
    let out = run_with(&["run", "--growth", "do something"]).await;
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(
        err.contains(
            "[growth] skill e2e-greet retired: policy drift: step 1 (write_file) would be blocked"
        ),
        "{err}"
    );
    assert!(
        !err.contains("[growth] skills:"),
        "no survivors registered: {err}"
    );

    // The retire event landed…
    let log = std::fs::read_to_string(workspace().join(".amparo/skills/adopted.jsonl")).unwrap();
    let row: Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
    assert_eq!(row["event"], "retire");

    // …and a later --allow-all task registers nothing: retirement holds.
    let mock = MockLlm::start(vec![vec![content_frame("Done.")]]).await;
    let (env, _p2) = mock_env(&mock).await;
    let out = run_with(&["run", "--growth", "--allow-all", "do something"]).await;
    drop(env);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(
        !err.contains("[growth] skills:"),
        "the retired skill stays retired: {err}"
    );
    restore_workspace_env(prior);
}

// ── Notebook rollup (M6e) ────────────────────────────────────────────────────

/// Seed one cold-archive row: a memory entry whose content is a run
/// record. Callers hold `LOCK` and have cleared `.amparo`. `started_at`
/// drives fold age and list order; `hash` is the tool-sequence hash
/// (dedupe identity).
fn seed_cold_row(id: &str, started_at: &str, hash: &str, task: &str) {
    let record = json!({
        "version": 1,
        "tenant_id": "cli",
        "started_at": started_at,
        "duration_ms": 100,
        "task_text": task,
        "tool_sequence_hash": hash,
        "tool_calls": [],
        "verification": {"decision": "complete", "feedback": null},
        "status": "complete",
        "final_answer": "Done.",
        "token_cost_estimate": 1,
    });
    let entry = json!({ "id": id, "content": record.to_string(), "created_at": started_at });
    let path = workspace().join(".amparo/notebook/records.jsonl");
    std::fs::create_dir_all(path.parent().expect("notebook dir")).expect("notebook dir");
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .expect("open cold archive");
    writeln!(file, "{entry}").expect("append cold row");
}

#[tokio::test]
async fn notebook_help_exits_0() {
    let _guard = LOCK.lock().await;
    let out = run_with(&["notebook", "--help"]).await;
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(stdout(&out).contains("amparo notebook"), "{}", stdout(&out));
    assert!(stdout(&out).contains("rollup"), "{}", stdout(&out));
    assert!(stdout(&out).contains("--dry-run"), "{}", stdout(&out));
}

#[tokio::test]
async fn notebook_surface_usage_errors_exit_2() {
    let _guard = LOCK.lock().await;
    let out = run_with(&["notebook"]).await;
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("missing subcommand"),
        "{}",
        stderr(&out)
    );
    let out = run_with(&["notebook", "bogus"]).await;
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("unknown notebook subcommand bogus"),
        "{}",
        stderr(&out)
    );
    let out = run_with(&["notebook", "promote"]).await;
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("requires a record id"),
        "{}",
        stderr(&out)
    );
}

#[tokio::test]
async fn growth_promotes_the_cold_tail_into_hot() {
    let _guard = LOCK.lock().await;
    let marker = format!("amparo-cli-e2e-{}", std::process::id());
    let mock = MockLlm::start(vec![
        tool_call_script(&format!("echo {marker}")),
        vec![content_frame("Done.")],
    ])
    .await;
    let (env, prior) = mock_env(&mock).await;
    std::fs::remove_dir_all(workspace().join(".amparo")).ok();

    let out = run_with(&[
        "run",
        "--allow-all",
        "--auto-approve",
        "--growth",
        "run the echo",
    ])
    .await;
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");

    let cold_path = workspace().join(".amparo/notebook/records.jsonl");
    let cold_text = std::fs::read_to_string(&cold_path).expect("cold archive");
    let cold_lines: Vec<&str> = cold_text.lines().collect();
    assert_eq!(cold_lines.len(), 1, "exactly one cold record: {cold_text}");
    let cold_entry: Value = serde_json::from_str(cold_lines[0]).expect("cold row is JSON");
    let cold_id = cold_entry["id"].as_str().expect("cold id").to_string();

    // The forced rollup promotes the tail into hot with the same id.
    let rollup = run_with(&["notebook", "rollup"]).await;
    let rollup_err = stderr(&rollup);
    assert_eq!(rollup.status.code(), Some(0), "stderr: {rollup_err}");
    assert!(
        rollup_err
            .contains("[notebook] rollup: promoted 1, folded 0, kept 0 record(s) in the hot layer"),
        "{rollup_err}"
    );

    let hot_text = std::fs::read_to_string(workspace().join(".amparo/notebook/hot.jsonl"))
        .expect("hot layer written");
    let hot_lines: Vec<&str> = hot_text.lines().collect();
    assert_eq!(hot_lines.len(), 1, "exactly one hot row: {hot_text}");
    let hot_entry: Value = serde_json::from_str(hot_lines[0]).expect("hot row is JSON");
    assert_eq!(
        hot_entry["id"].as_str().expect("hot id"),
        cold_id,
        "the hot row keeps the cold id"
    );
    let hot_record: Value =
        serde_json::from_str(hot_entry["content"].as_str().expect("content")).expect("record JSON");
    assert_eq!(hot_record["tenant_id"], "cli");

    let hashes = std::fs::read_to_string(workspace().join(".amparo/notebook/hot-hashes.jsonl"))
        .expect("hash sidecar written");
    assert_eq!(hashes.lines().count(), 1, "one hash row: {hashes}");
    assert!(
        workspace().join(".amparo/notebook/rollup.json").exists(),
        "rollup state saved"
    );
    // The cold archive is the record: the rollup never touches it.
    let cold_after = std::fs::read_to_string(&cold_path).expect("cold archive still there");
    assert_eq!(
        cold_after.lines().count(),
        1,
        "cold unchanged: {cold_after}"
    );

    restore_workspace_env(prior);
    drop(env);
}

#[tokio::test]
async fn growth_promotion_dedupes_by_sequence_hash() {
    let _guard = LOCK.lock().await;
    let mock = MockLlm::start(vec![vec![content_frame("Done.")]]).await;
    let (env, prior) = mock_env(&mock).await;
    std::fs::remove_dir_all(workspace().join(".amparo")).ok();

    // Two different text-only tasks share the empty tool-sequence hash:
    // the second is a dedupe duplicate of the first.
    let out_a = run_with(&[
        "run",
        "--allow-all",
        "--auto-approve",
        "--growth",
        "first task",
    ])
    .await;
    assert_eq!(out_a.status.code(), Some(0), "stderr: {}", stderr(&out_a));
    let out_b = run_with(&[
        "run",
        "--allow-all",
        "--auto-approve",
        "--growth",
        "second task",
    ])
    .await;
    let err_b = stderr(&out_b);
    assert_eq!(out_b.status.code(), Some(0), "stderr: {err_b}");
    // Run B's start promoted run A's record from the cold tail.
    assert!(
        err_b.contains("[growth] notebook: promoted 1 tail record(s) to the hot layer"),
        "{err_b}"
    );

    // The forced rollup finds B a duplicate of A: one hot row for two cold.
    let rollup = run_with(&["notebook", "rollup"]).await;
    let rollup_err = stderr(&rollup);
    assert_eq!(rollup.status.code(), Some(0), "stderr: {rollup_err}");
    assert!(
        rollup_err.contains("promoted 0, folded 0, kept 1"),
        "{rollup_err}"
    );
    let cold = std::fs::read_to_string(workspace().join(".amparo/notebook/records.jsonl"))
        .expect("cold archive");
    assert_eq!(cold.lines().count(), 2, "two cold records: {cold}");
    let hot =
        std::fs::read_to_string(workspace().join(".amparo/notebook/hot.jsonl")).expect("hot layer");
    assert_eq!(hot.lines().count(), 1, "one deduped hot row: {hot}");
    let hashes = std::fs::read_to_string(workspace().join(".amparo/notebook/hot-hashes.jsonl"))
        .expect("hash sidecar");
    assert_eq!(hashes.lines().count(), 1, "one hash row: {hashes}");

    restore_workspace_env(prior);
    drop(env);
}

#[tokio::test]
async fn notebook_promote_keeps_a_case_through_fold() {
    let _guard = LOCK.lock().await;
    let prior = set_workspace_env();
    std::fs::remove_dir_all(workspace().join(".amparo")).ok();

    // Two old records with the same (empty) tool-sequence hash — different
    // text-only tasks of the same shape. Old enough that the 90-day fold
    // would drop them (today is 2026-08-29).
    seed_cold_row("rec-a", "2026-01-01T00:00:00Z", "empty", "first old task");
    seed_cold_row("rec-b", "2026-01-02T00:00:00Z", "empty", "second old task");

    let out = run_with(&["notebook", "promote", "rec-a"]).await;
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(
        err.contains("[notebook] promoted rec-a to the hot layer"),
        "{err}"
    );

    // The forced rollup folds old rows — the promoted one is exempt, and
    // its hash row already represents the duplicate, so hot keeps only the
    // promoted row.
    let rollup = run_with(&["notebook", "rollup"]).await;
    let rollup_err = stderr(&rollup);
    assert_eq!(rollup.status.code(), Some(0), "stderr: {rollup_err}");
    assert!(
        rollup_err.contains("promoted 0, folded 0, kept 1"),
        "{rollup_err}"
    );
    let hot =
        std::fs::read_to_string(workspace().join(".amparo/notebook/hot.jsonl")).expect("hot layer");
    let rows: Vec<&str> = hot.lines().collect();
    assert_eq!(rows.len(), 1, "only the promoted row survives: {hot}");
    let entry: Value = serde_json::from_str(rows[0]).expect("hot row is JSON");
    assert_eq!(entry["id"], "rec-a", "the fold-exempt promoted row: {hot}");

    restore_workspace_env(prior);
}

#[tokio::test]
async fn notebook_promote_already_promoted_exits_0() {
    let _guard = LOCK.lock().await;
    let prior = set_workspace_env();
    std::fs::remove_dir_all(workspace().join(".amparo")).ok();
    seed_cold_row("rec-a", "2026-08-01T00:00:00Z", "hash-a", "a task");

    let out = run_with(&["notebook", "promote", "rec-a"]).await;
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let out = run_with(&["notebook", "promote", "rec-a"]).await;
    let err = stderr(&out);
    assert_eq!(
        out.status.code(),
        Some(0),
        "idempotent promotion exits 0: {err}"
    );
    assert!(
        err.contains("[notebook] rec-a is already promoted to the hot layer"),
        "{err}"
    );
    restore_workspace_env(prior);
}

#[tokio::test]
async fn notebook_promote_unknown_id_exits_1() {
    let _guard = LOCK.lock().await;
    let prior = set_workspace_env();
    std::fs::remove_dir_all(workspace().join(".amparo")).ok();
    seed_cold_row("rec-a", "2026-08-01T00:00:00Z", "hash-a", "a task");

    let out = run_with(&["notebook", "promote", "rec-404"]).await;
    let err = stderr(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "unknown id is a runtime failure: {err}"
    );
    assert!(err.contains("unknown record id rec-404"), "{err}");
    restore_workspace_env(prior);
}

#[tokio::test]
async fn notebook_rollup_dry_run_writes_nothing() {
    let _guard = LOCK.lock().await;
    let prior = set_workspace_env();
    std::fs::remove_dir_all(workspace().join(".amparo")).ok();
    seed_cold_row("rec-a", "2026-08-01T00:00:00Z", "hash-a", "a task");

    let out = run_with(&["notebook", "rollup", "--dry-run"]).await;
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(
        err.contains(
            "[notebook] rollup: promoted 1, folded 0, kept 0 record(s) in the hot layer \
             (dry-run — nothing written)"
        ),
        "{err}"
    );
    assert!(
        !workspace().join(".amparo/notebook/hot.jsonl").exists(),
        "dry-run writes no hot rows"
    );
    assert!(
        !workspace()
            .join(".amparo/notebook/hot-hashes.jsonl")
            .exists(),
        "dry-run writes no hash rows"
    );
    assert!(
        !workspace().join(".amparo/notebook/rollup.json").exists(),
        "dry-run writes no state"
    );
    restore_workspace_env(prior);
}

#[tokio::test]
async fn notebook_list_shows_records_with_promotion_state() {
    let _guard = LOCK.lock().await;
    let prior = set_workspace_env();
    std::fs::remove_dir_all(workspace().join(".amparo")).ok();
    seed_cold_row("rec-old", "2026-08-01T00:00:00Z", "hash-old", "older task");
    seed_cold_row("rec-new", "2026-08-02T00:00:00Z", "hash-new", "newer task");

    let out = run_with(&["notebook", "promote", "rec-new"]).await;
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));

    let out = run_with(&["notebook", "list"]).await;
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    let out_stdout = stdout(&out);
    let lines: Vec<&str> = out_stdout.lines().collect();
    assert_eq!(lines.len(), 2, "two listed records: {out_stdout}");
    assert!(
        lines[0].starts_with("rec-new") && lines[0].contains("  [promoted]"),
        "newest first, promoted flagged: {out_stdout}"
    );
    assert!(lines[0].contains("complete/complete"), "{}", out_stdout);
    assert!(lines[0].contains("newer task"), "{}", out_stdout);
    assert!(lines[1].starts_with("rec-old"), "{out_stdout}");
    assert!(!lines[1].contains("[promoted]"), "{out_stdout}");

    // An unknown tenant lists nothing, quietly.
    let out = run_with(&["notebook", "list", "--tenant", "nobody"]).await;
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(stdout(&out).is_empty(), "{}", stdout(&out));
    assert!(err.contains("no records for tenant nobody"), "{err}");
    restore_workspace_env(prior);
}

// ── Privacy ledger (M7) ───────────────────────────────────────────────────────

#[tokio::test]
async fn privacy_help_exits_0() {
    let _guard = LOCK.lock().await;
    let out = run_with(&["privacy", "--help"]).await;
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(stdout(&out).contains("amparo privacy"), "{}", stdout(&out));
    assert!(stdout(&out).contains("--last"), "{}", stdout(&out));
    assert!(stdout(&out).contains("--tenant"), "{}", stdout(&out));
}

#[tokio::test]
async fn privacy_surface_usage_errors_exit_2() {
    let _guard = LOCK.lock().await;
    let out = run_with(&["privacy", "--nonsense"]).await;
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("unknown flag --nonsense"),
        "{}",
        stderr(&out)
    );
    let out = run_with(&["privacy", "--last", "0"]).await;
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("positive integer"),
        "{}",
        stderr(&out)
    );
    let out = run_with(&["privacy", "positional"]).await;
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("takes no positional arguments"),
        "{}",
        stderr(&out)
    );
}

#[tokio::test]
async fn privacy_without_a_ledger_reports_empty() {
    let _guard = LOCK.lock().await;
    let prior = set_workspace_env();
    std::fs::remove_dir_all(workspace().join(".amparo")).ok();

    // Reading must never create the ledger: an empty workspace reports
    // empty and exits 0.
    let out = run_with(&["privacy"]).await;
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(err.contains("[privacy] no ledger for tenant cli"), "{err}");
    assert!(stdout(&out).is_empty(), "{}", stdout(&out));
    assert!(
        !workspace().join(".amparo/privacy/ledger.jsonl").exists(),
        "a read never creates the ledger"
    );
    restore_workspace_env(prior);
}

#[tokio::test]
async fn privacy_subcommand_reports_ledger_after_run() {
    let _guard = LOCK.lock().await;
    // A refused localhost port fails instantly and deterministically —
    // the ledger records the attempt either way, and the loop continues
    // to the final answer.
    let mock = MockLlm::start(vec![
        tool_script(
            "fetch_url",
            "{\"url\":\"http://127.0.0.1:1/path?q=supersecret\"}",
        ),
        vec![content_frame("Done.")],
    ])
    .await;
    let (env, prior) = mock_env(&mock).await;
    std::fs::remove_dir_all(workspace().join(".amparo")).ok();

    // No --growth: the ledger is always-on and writes anyway.
    let out = run_with(&["run", "--allow-all", "--auto-approve", "fetch the page"]).await;
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");

    let ledger = workspace().join(".amparo/privacy/ledger.jsonl");
    assert!(
        ledger.exists(),
        "the always-on ledger exists without --growth"
    );
    let text = std::fs::read_to_string(&ledger).expect("ledger exists");
    assert!(text.contains("fetch_url"), "tool recorded: {text}");
    assert!(text.contains("http://127.0.0.1:1"), "host kept: {text}");
    assert!(
        !text.contains("supersecret"),
        "query never reaches the ledger: {text}"
    );
    assert!(
        !text.contains("/path"),
        "path never reaches the ledger: {text}"
    );

    // The reviewer surface: summary + tail, host only — no query.
    let out = run_with(&["privacy"]).await;
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    let out_stdout = stdout(&out);
    assert!(out_stdout.contains("tenant cli"), "{out_stdout}");
    assert!(out_stdout.contains("network calls: 1"), "{out_stdout}");
    assert!(out_stdout.contains("tools: fetch_url x1"), "{out_stdout}");
    assert!(
        out_stdout.contains("http://127.0.0.1:1"),
        "the row shows the host: {out_stdout}"
    );
    assert!(
        !out_stdout.contains("supersecret"),
        "query never shown: {out_stdout}"
    );
    assert!(
        !out_stdout.contains("/path"),
        "path never shown: {out_stdout}"
    );

    restore_workspace_env(prior);
    drop(env);
}

#[tokio::test]
async fn run_with_tiny_ledger_quota_rotates_and_privacy_reports_it() {
    let _guard = LOCK.lock().await;
    // One ~180-byte row per call against a 250-byte quota: after the
    // first append every further append rotates, dropping the previous
    // row + previous marker — steady state is [newest row, marker
    // recording dropped 2], ~270 bytes.
    let mut scripts: Vec<Script> = (1..=8)
        .map(|n| {
            tool_script(
                "fetch_url",
                &format!("{{\"url\":\"http://127.0.0.1:1/{n}\"}}"),
            )
        })
        .collect();
    scripts.push(vec![content_frame("Done.")]);
    let mock = MockLlm::start(scripts).await;
    let (env, prior) = mock_env(&mock).await;
    std::fs::remove_dir_all(workspace().join(".amparo")).ok();

    let out = run_with(&[
        "run",
        "--allow-all",
        "--auto-approve",
        "--ledger-max-bytes",
        "250",
        "fetch pages",
    ])
    .await;
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(stdout(&out).contains("Done."), "{}", stdout(&out));

    let ledger = workspace().join(".amparo/privacy/ledger.jsonl");
    let text = std::fs::read_to_string(&ledger).expect("ledger exists");
    let row_count = text.matches(r#""kind":"network_call""#).count();
    assert_eq!(row_count, 1, "rotation keeps one surviving row: {text}");
    assert!(
        text.contains(r#""kind":"rotated""#),
        "marker row present: {text}"
    );
    assert!(
        text.contains(r#""dropped_rows":2"#),
        "steady state drops the previous row + previous marker: {text}"
    );
    let bytes = std::fs::metadata(&ledger)
        .map(|m| m.len())
        .unwrap_or(u64::MAX);
    assert!(
        bytes < 600,
        "the bounded ledger stays well under a kilobyte: {bytes} bytes"
    );
    // The quota sidecar is how the reviewer surface reports the bound.
    assert_eq!(
        std::fs::read_to_string(workspace().join(".amparo/privacy/quota"))
            .map(|s| s.trim().to_string())
            .ok(),
        Some("250".to_string())
    );

    // `amparo privacy` reports the bound, the rotation and the drop —
    // and the marker row renders in the tail.
    let out = run_with(&["privacy"]).await;
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    let out_stdout = stdout(&out);
    assert!(out_stdout.contains("quota 250"), "{out_stdout}");
    assert!(out_stdout.contains("rotations: 1"), "{out_stdout}");
    assert!(out_stdout.contains("rows dropped 2"), "{out_stdout}");
    assert!(
        out_stdout.contains("rotated  dropped 2 rows"),
        "{out_stdout}"
    );

    restore_workspace_env(prior);
    drop(env);
}

#[tokio::test]
async fn run_ledger_max_bytes_garbage_exits_2() {
    let _guard = LOCK.lock().await;
    // Usage errors never reach the LLM or the ledger: parse first.
    let out = run_with(&["run", "--ledger-max-bytes", "banana", "x"]).await;
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("--ledger-max-bytes"),
        "{}",
        stderr(&out)
    );
    let out = run_with(&["run", "--ledger-max-bytes", "0", "x"]).await;
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("must be positive"),
        "{}",
        stderr(&out)
    );
}

#[tokio::test]
async fn run_with_unwritable_ledger_warns_and_continues() {
    let _guard = LOCK.lock().await;
    let mock = MockLlm::start(vec![vec![content_frame("Done.")]]).await;
    let (env, prior) = mock_env(&mock).await;
    // A FILE named .amparo blocks the ledger directory — the run must
    // warn and continue without the ledger, never fail.
    std::fs::remove_dir_all(workspace().join(".amparo")).ok();
    std::fs::write(workspace().join(".amparo"), "blocking file").unwrap();

    let out = run_with(&["run", "--allow-all", "--auto-approve", "plain run"]).await;

    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(
        err.contains("[ledger] unavailable"),
        "open failure warns: {err}"
    );
    assert!(
        err.contains("checkpoint save failed"),
        "the checkpoint store's failure also warns, never fatal: {err}"
    );
    assert_eq!(stdout(&out).trim(), "Done.");

    // Clean up so later tests can create the directory.
    std::fs::remove_file(workspace().join(".amparo")).unwrap();
}

// ── M7 W7: `amparo run --resume` ─────────────────────────────────────────────

/// Set `AMPARO_WORKSPACE` to a specific directory for one test; the return
/// restores whatever was set before (see [`restore_workspace_env`]).
fn set_workspace_env_to(dir: &std::path::Path) -> Option<String> {
    let prior = std::env::var("AMPARO_WORKSPACE").ok();
    std::env::set_var("AMPARO_WORKSPACE", dir);
    prior
}

/// A fresh, empty workspace for one test. The shared [`workspace`] root is
/// reused by many tests, but resume tests need a controlled checkpoint dir.
fn fresh_workspace(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("amparo-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Unix seconds now.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Write a `Running` checkpoint for tenant `cli` under `ws`, in the
/// agent's own W6 layout — the fixture a resume replays.
fn write_checkpoint(ws: &std::path::Path, task_id: &str, started_at: u64, steps_used: usize) {
    let dir = ws.join(".amparo").join("sessions").join("cli");
    std::fs::create_dir_all(&dir).unwrap();
    let checkpoint = json!({
        "version": 1,
        "tenant": "cli",
        "task_id": task_id,
        "started_at": started_at,
        "prompt": "fixture task",
        "status": "running",
        "conversation": [{"role": "user", "content": "fixture task"}],
        "loop_state": {
            "last_tool_name": null,
            "same_tool_count": 0,
            "empty_turn_retried": false,
            "last_good_summary": null,
            "used_tool_names": [],
            "steps_used": steps_used
        },
        "final_answer": null
    });
    std::fs::write(dir.join(format!("{task_id}.json")), checkpoint.to_string()).unwrap();
}

#[test]
fn resume_with_a_task_exits_2() {
    let out = std::process::Command::new(bin())
        .args(["run", "--resume", "a task"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("takes no task"), "{}", stderr(&out));
}

#[tokio::test]
async fn resume_without_a_checkpoint_exits_1() {
    let _guard = LOCK.lock().await;
    let ws = fresh_workspace("resume-none");
    // No inference env: the checkpoint lookup precedes any wiring, so a
    // missing session must fail on its own, not on the environment.
    let env = set_env(&[], &["AMPARO_INFERENCE_URL", "AMPARO_INFERENCE_MODEL"]);
    let prior = set_workspace_env_to(&ws);
    let out = run_with(&["run", "--resume"]).await;
    restore_workspace_env(prior);
    drop(env);

    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("no incomplete checkpoint for tenant cli"),
        "{}",
        stderr(&out)
    );
}

#[tokio::test]
async fn resume_replays_a_checkpoint_to_completion() {
    let _guard = LOCK.lock().await;
    let ws = fresh_workspace("resume-replay");
    write_checkpoint(&ws, "sess-1", unix_now(), 2);

    let mock = MockLlm::start(vec![vec![content_frame("Resumed and done.")]]).await;
    let env_pairs = mock.env();
    let vars: Vec<(&str, &str)> = env_pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let env = set_env(&vars, &[]);
    let prior = set_workspace_env_to(&ws);
    let out = run_with(&["run", "--resume", "--allow-all"]).await;
    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(err.contains("[session] resumed sess-1 (step 2)"), "{err}");
    // A resume is a run start too: the power-on banner greets it.
    assert!(err.contains("[wake] Amparo is awake."), "{err}");
    assert_eq!(stdout(&out).trim(), "Resumed and done.");
    // The resumed agent replaced the Running checkpoint with a terminal
    // one in place — same file, now complete.
    let saved: Value = serde_json::from_str(
        &std::fs::read_to_string(
            ws.join(".amparo")
                .join("sessions")
                .join("cli")
                .join("sess-1.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(saved["status"], "complete");
    assert!(
        saved["final_answer"].is_string(),
        "terminal checkpoint carries the answer"
    );
}

#[tokio::test]
async fn resume_skips_a_stale_checkpoint_with_a_warn() {
    let _guard = LOCK.lock().await;
    let ws = fresh_workspace("resume-stale");
    write_checkpoint(&ws, "sess-old", unix_now() - 8 * 86_400, 1);
    let env = set_env(&[], &["AMPARO_INFERENCE_URL", "AMPARO_INFERENCE_MODEL"]);
    let prior = set_workspace_env_to(&ws);
    let out = run_with(&["run", "--resume"]).await;
    restore_workspace_env(prior);
    drop(env);

    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("[session] skipping stale running checkpoint sess-old"),
        "{}",
        stderr(&out)
    );
    // The stale checkpoint is untouched — still Running, still one file.
    let saved: Value = serde_json::from_str(
        &std::fs::read_to_string(
            ws.join(".amparo")
                .join("sessions")
                .join("cli")
                .join("sess-old.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(saved["status"], "running");
}

// ── M7 W9: cross-feature e2e ────────────────────────────────────────────────

#[tokio::test]
async fn resume_lands_ledger_growth_and_checkpoint_together() {
    let _guard = LOCK.lock().await;
    let ws = fresh_workspace("w9-resume-cross");
    write_checkpoint(&ws, "sess-1", unix_now(), 2);

    let mock = MockLlm::start(vec![
        tool_script("fetch_url", "{\"url\":\"http://127.0.0.1:1/w9-cross\"}"),
        vec![content_frame("Done.")],
    ])
    .await;
    let env_pairs = mock.env();
    let vars: Vec<(&str, &str)> = env_pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let env = set_env(&vars, &[]);
    let prior = set_workspace_env_to(&ws);
    let out = run_with(&[
        "run",
        "--resume",
        "--allow-all",
        "--auto-approve",
        "--growth",
    ])
    .await;
    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(err.contains("[session] resumed sess-1 (step 2)"), "{err}");
    assert_eq!(stdout(&out).trim(), "Done.");

    // Ledger (always-on) recorded the resumed task's fetch attempt.
    let ledger = ws.join(".amparo/privacy/ledger.jsonl");
    let ledger_text = std::fs::read_to_string(&ledger).expect("ledger exists");
    assert!(ledger_text.contains("fetch_url"), "{ledger_text}");
    assert!(ledger_text.contains("http://127.0.0.1:1"), "{ledger_text}");
    assert!(
        !ledger_text.contains("w9-cross"),
        "the path never reaches the ledger: {ledger_text}"
    );

    // Growth wired on the resumed task — startup lines prove the notebook
    // and case retrieval attached. (A resume opens no NEW record: the
    // checkpoint is the session trail — locked in W6.)
    assert!(
        err.contains("[growth] recording PII-stripped run records to"),
        "the growth notebook attached: {err}"
    );
    assert!(
        err.contains("[growth] retrieval: prior cli cases (hot layer) inform self-verification"),
        "the case library attached: {err}"
    );

    // The resume replaced the Running checkpoint with the terminal one.
    let saved: Value = serde_json::from_str(
        &std::fs::read_to_string(ws.join(".amparo/sessions/cli/sess-1.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["status"], "complete");
    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn destructive_call_denied_shows_radius_and_lands_a_human_denied_row() {
    let _guard = LOCK.lock().await;
    let marker = format!("amparo-w9-{}", std::process::id());
    let mock = MockLlm::start(vec![
        tool_call_script(&format!("rm -rf /tmp/{marker}")),
        vec![content_frame("Done.")],
    ])
    .await;
    let (env, prior) = mock_env(&mock).await;
    std::fs::remove_dir_all(workspace().join(".amparo")).ok();

    let out = run_with_stdin(&["run", "--allow-all", "clean up the temp dir"], b"n\n").await;

    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(
        err.contains("[preflight] blast radius: destructive"),
        "the prompt names the concrete consequence: {err}"
    );
    assert!(err.contains("[approval] denied"), "{err}");
    assert_eq!(stdout(&out).trim(), "Done.");

    // The denial itself is the ledger row: the loop never executes a
    // denied call, but the audit question has its answer.
    let ledger = workspace().join(".amparo/privacy/ledger.jsonl");
    let text = std::fs::read_to_string(&ledger).expect("ledger exists");
    assert!(text.contains(r#""gate":"human_denied""#), "{text}");
    assert!(text.contains(r#""outcome":"denied""#), "{text}");
    assert!(text.contains(r#""tool":"run_command""#), "{text}");
    assert!(
        !text.contains("rm -rf"),
        "the command never reaches the ledger: {text}"
    );
}

// ── Schedule queue (M8 W5) ───────────────────────────────────────────────────

/// Seed one pending promise in `ws`'s queue — the shape the chat driver's
/// `schedule` tool persists, hand-written here so the CLI surface is
/// exercised against a real queue dir.
fn seed_promise(ws: &std::path::Path, id: &str, at: &str) {
    let store = JsonScheduleStore::new(schedule_dir(ws));
    let task = ScheduledTask {
        id: id.to_string(),
        tenant: "telegram:user_1".to_string(),
        platform: "telegram".to_string(),
        chat_id: "chat_1".to_string(),
        requester: "user_1".to_string(),
        task: "run the standing task".to_string(),
        at: at.to_string(),
        status: ScheduledStatus::Pending,
        result: None,
    };
    store.save(&task).unwrap();
}

/// Seed one pending promise in `ws`'s queue written by the CLI itself
/// (M10 W5) — the shape `ScheduleTool::for_cli` persists: tenant and
/// platform `cli`, the run's task id as chat id and requester.
fn seed_cli_promise(ws: &std::path::Path, id: &str, at: &str) {
    let store = JsonScheduleStore::new(schedule_dir(ws));
    let task = ScheduledTask {
        id: id.to_string(),
        tenant: "cli".to_string(),
        platform: "cli".to_string(),
        chat_id: "sess-parent".to_string(),
        requester: "sess-parent".to_string(),
        task: "run the standing task".to_string(),
        at: at.to_string(),
        status: ScheduledStatus::Pending,
        result: None,
    };
    store.save(&task).unwrap();
}

#[test]
fn schedule_help_exits_0() {
    let out = std::process::Command::new(bin())
        .args(["schedule", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = stdout(&out);
    assert!(stdout.contains("amparo schedule"), "{}", stdout);
    assert!(stdout.contains("cancel"), "{}", stdout);
    assert!(stdout.contains("--workspace"), "{}", stdout);
}

#[tokio::test]
async fn schedule_surface_usage_errors_exit_2() {
    let _guard = LOCK.lock().await;
    let out = run_with(&["schedule"]).await;
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("requires a command"),
        "{}",
        stderr(&out)
    );
    let out = run_with(&["schedule", "frobnicate"]).await;
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("unknown schedule command 'frobnicate'"),
        "{}",
        stderr(&out)
    );
    let out = run_with(&["schedule", "cancel"]).await;
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("requires an id"), "{}", stderr(&out));
    let out = run_with(&["schedule", "list", "extra"]).await;
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("takes no arguments"),
        "{}",
        stderr(&out)
    );
}

#[tokio::test]
async fn schedule_list_and_cancel_work_the_queue() {
    let _guard = LOCK.lock().await;
    let ws = fresh_workspace("schedule-e2e");
    seed_promise(&ws, "sched-e2e-1", "2026-09-15T12:00:00Z");
    let ws_flag = ws.to_str().expect("utf8 temp path");

    // list shows the pending promise: id, instant, status, tenant, task.
    let out = run_with(&["schedule", "list", "--workspace", ws_flag]).await;
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    let out_stdout = stdout(&out);
    assert!(out_stdout.contains("sched-e2e-1"), "{out_stdout}");
    assert!(out_stdout.contains("2026-09-15T12:00:00Z"), "{out_stdout}");
    assert!(out_stdout.contains("pending"), "{out_stdout}");
    assert!(out_stdout.contains("telegram:user_1"), "{out_stdout}");
    assert!(out_stdout.contains("run the standing task"), "{out_stdout}");

    // cancel moves the promise to cancelled — the file survives (a status
    // change, never a deletion).
    let out = run_with(&["schedule", "cancel", "sched-e2e-1", "--workspace", ws_flag]).await;
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(
        stdout(&out).contains("cancelled sched-e2e-1"),
        "{}",
        stdout(&out)
    );
    let saved: Value = serde_json::from_str(
        &std::fs::read_to_string(ws.join(".amparo/schedule/sched-e2e-1.json"))
            .expect("the queue file survives"),
    )
    .expect("queue file is JSON");
    assert_eq!(saved["status"], "cancelled");
    assert_eq!(saved["result"], "cancelled by the operator");

    // A second cancel is a runtime failure: only a pending promise can
    // be cancelled.
    let out = run_with(&["schedule", "cancel", "sched-e2e-1", "--workspace", ws_flag]).await;
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("only a pending promise can be cancelled"),
        "{}",
        stderr(&out)
    );

    // An unknown id is a runtime failure too.
    let out = run_with(&["schedule", "cancel", "sched-404", "--workspace", ws_flag]).await;
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("no such schedule: sched-404"),
        "{}",
        stderr(&out)
    );

    // An empty queue lists nothing, and a read never creates the dir.
    let empty = fresh_workspace("schedule-empty");
    let out = run_with(&["schedule", "list", "--workspace", empty.to_str().unwrap()]).await;
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(
        stdout(&out).contains("no scheduled tasks in"),
        "{}",
        stdout(&out)
    );
    assert!(
        !empty.join(".amparo/schedule").exists(),
        "a list never creates the queue dir"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

// ── M10 W5: the CLI scheduler — due_scan at run start ────────────────────────

#[tokio::test]
async fn run_schedules_a_stripped_cli_promise() {
    let _guard = LOCK.lock().await;
    let ws = fresh_workspace("w5-schedule-tool");
    let mock = MockLlm::start(vec![
        tool_script(
            "schedule",
            r#"{"at":"2099-01-01T00:00:00Z","task":"email alice@example.com the standing report"}"#,
        ),
        vec![content_frame("Done.")],
    ])
    .await;
    let (env, prior) = mock_env(&mock).await;
    let ws_prior = set_workspace_env_to(&ws);
    let out = run_with(&["run", "--allow-all", "--auto-approve", "schedule something"]).await;
    restore_workspace_env(ws_prior);
    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert_eq!(stdout(&out).trim(), "Done.");

    // The promise persisted to the workspace queue as the CLI wrote it:
    // tenant and platform `cli`, the run's own task id as requester, and
    // the task PII-stripped before it ever touches the queue (I6).
    let tasks = JsonScheduleStore::new(schedule_dir(&ws)).load_all();
    assert_eq!(tasks.len(), 1, "{err}");
    let task = &tasks[0];
    assert_eq!(task.status, ScheduledStatus::Pending);
    assert_eq!(task.tenant, "cli");
    assert_eq!(task.platform, "cli");
    assert_eq!(task.chat_id, task.requester);
    assert!(
        task.requester.starts_with("sess-"),
        "the requester is the run's task id: {}",
        task.requester
    );
    assert!(
        task.task.contains("[EMAIL_1]"),
        "the persisted task is PII-stripped: {}",
        task.task
    );
    assert!(
        !task.task.contains("alice@example.com"),
        "the raw address never reaches the queue: {}",
        task.task
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn a_due_cli_promise_fires_at_run_start() {
    let _guard = LOCK.lock().await;
    let ws = fresh_workspace("w5-due-fire");
    // Seeded 30s in the past — inside the 60s grace window, so the
    // run-start scan fires it (the chat driver's own fire-test precedent
    // seeds the same way).
    let at = (chrono::Utc::now() - chrono::Duration::seconds(30)).to_rfc3339();
    seed_cli_promise(&ws, "sched-fire-1", &at);

    // One repeating script: the main task and the concurrently-fired
    // promise both answer "Done." off the same mock.
    let mock = MockLlm::start(vec![vec![content_frame("Done.")]]).await;
    let (env, prior) = mock_env(&mock).await;
    let ws_prior = set_workspace_env_to(&ws);
    let out = run_with(&["run", "--allow-all", "say hello"]).await;
    restore_workspace_env(ws_prior);
    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(stdout(&out).contains("Done."), "{}", stdout(&out));
    assert!(
        err.contains("[schedule] sched-fire-1 fired"),
        "the fire is reported on stderr: {err}"
    );

    // The promise record carries the fire's outcome — awaited before the
    // process exited, so the record is on disk.
    let task = JsonScheduleStore::new(schedule_dir(&ws))
        .load("sched-fire-1")
        .unwrap()
        .expect("the promise record exists");
    assert_eq!(task.status, ScheduledStatus::Fired);
    let result = task.result.clone().unwrap_or_default();
    assert!(
        result.contains("Done."),
        "the fire's answer lands on the promise: {result}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn a_due_cli_promise_fires_through_the_gate_chain() {
    let _guard = LOCK.lock().await;
    let ws = fresh_workspace("w6-due-fire-gate");
    let at = (chrono::Utc::now() - chrono::Duration::seconds(30)).to_rfc3339();
    seed_cli_promise(&ws, "sched-fire-1", &at);

    // Two write_file turns then two answers: the fire and the main task
    // each make exactly one gated write_file call (the queue hands the
    // tools out first, whichever task arrives), then both finish — so
    // the fire's call is provably among the gated rows below.
    let mock = MockLlm::start(vec![
        tool_script(
            "write_file",
            r#"{"path":"fire.txt","content":"fire content"}"#,
        ),
        tool_script(
            "write_file",
            r#"{"path":"main.txt","content":"main content"}"#,
        ),
        vec![content_frame("Done.")],
    ])
    .await;
    let (env, prior) = mock_env(&mock).await;
    let ws_prior = set_workspace_env_to(&ws);
    let out = run_with(&["run", "--allow-all", "say hello"]).await;
    restore_workspace_env(ws_prior);
    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert_eq!(
        err.matches("[gate] write_file: allowed").count(),
        2,
        "both gated calls go through the chain: {err}"
    );
    assert_eq!(err.matches("[exec] write_file").count(), 2, "{err}");
    // The two gated calls actually executed — both files landed.
    assert!(ws.join("fire.txt").exists(), "{err}");
    assert!(ws.join("main.txt").exists(), "{err}");
    assert!(err.contains("[schedule] sched-fire-1 fired"), "{err}");
    let task = JsonScheduleStore::new(schedule_dir(&ws))
        .load("sched-fire-1")
        .unwrap()
        .expect("the promise record exists");
    assert_eq!(task.status, ScheduledStatus::Fired);
    assert!(
        task.result.clone().unwrap_or_default().contains("Done."),
        "the fire's answer lands on the promise: {:?}",
        task.result
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn a_due_fire_that_needs_approval_is_denied_and_executes_nothing() {
    let _guard = LOCK.lock().await;
    let ws = fresh_workspace("w6-due-fire-denied");
    let at = (chrono::Utc::now() - chrono::Duration::seconds(30)).to_rfc3339();
    seed_cli_promise(&ws, "sched-fire-1", &at);

    // Two send_notification turns (ExternalEffector — always asks a
    // human), then two answers. stdin is closed and no --auto-* flag:
    // both calls must be denied and neither may execute — an unattended
    // fire never runs ahead of the gate (I1).
    let mock = MockLlm::start(vec![
        tool_script(
            "send_notification",
            r#"{"destination":"ops","message":"fire one"}"#,
        ),
        tool_script(
            "send_notification",
            r#"{"destination":"ops","message":"fire two"}"#,
        ),
        vec![content_frame("Done.")],
    ])
    .await;
    let (env, prior) = mock_env(&mock).await;
    let ws_prior = set_workspace_env_to(&ws);
    let out = run_with(&["run", "--allow-all", "say hello"]).await;
    restore_workspace_env(ws_prior);
    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert_eq!(
        err.matches("[gate] send_notification: approval_denied")
            .count(),
        2,
        "both calls are denied: {err}"
    );
    assert!(
        !err.contains("[exec] send_notification"),
        "denied calls never execute: {err}"
    );
    assert!(err.contains("[schedule] sched-fire-1 fired"), "{err}");
    let task = JsonScheduleStore::new(schedule_dir(&ws))
        .load("sched-fire-1")
        .unwrap()
        .expect("the promise record exists");
    assert_eq!(task.status, ScheduledStatus::Fired);
    assert!(
        task.result.clone().unwrap_or_default().contains("Done."),
        "the fire's answer lands on the promise: {:?}",
        task.result
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn a_due_cli_promise_fires_on_resume_too() {
    let _guard = LOCK.lock().await;
    let ws = fresh_workspace("w6-due-fire-resume");
    let at = (chrono::Utc::now() - chrono::Duration::seconds(30)).to_rfc3339();
    seed_cli_promise(&ws, "sched-fire-1", &at);
    write_checkpoint(&ws, "sess-1", unix_now(), 1);

    let mock = MockLlm::start(vec![vec![content_frame("Resumed and done.")]]).await;
    let (env, prior) = mock_env(&mock).await;
    let ws_prior = set_workspace_env_to(&ws);
    let out = run_with(&["run", "--resume", "--allow-all"]).await;
    restore_workspace_env(ws_prior);
    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert_eq!(stdout(&out).trim(), "Resumed and done.");
    assert!(
        err.contains("[schedule] sched-fire-1 fired"),
        "the resume path scans and fires like a fresh run: {err}"
    );
    let task = JsonScheduleStore::new(schedule_dir(&ws))
        .load("sched-fire-1")
        .unwrap()
        .expect("the promise record exists");
    assert_eq!(task.status, ScheduledStatus::Fired);

    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn an_overdue_cli_promise_is_marked_missed_and_chat_promises_are_untouched() {
    let _guard = LOCK.lock().await;
    let ws = fresh_workspace("w5-missed");
    let at = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    seed_cli_promise(&ws, "sched-miss-1", &at);
    // A chat promise in the same queue dir is none of the CLI's business
    // (I2): the chat host's ticker owns it, so the scan must skip it.
    seed_promise(&ws, "sched-chat-1", &at);

    let mock = MockLlm::start(vec![vec![content_frame("Done.")]]).await;
    let (env, prior) = mock_env(&mock).await;
    let ws_prior = set_workspace_env_to(&ws);
    let out = run_with(&["run", "--allow-all", "say hello"]).await;
    restore_workspace_env(ws_prior);
    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(
        err.contains("[schedule] sched-miss-1 missed"),
        "the overdue promise is reported missed: {err}"
    );
    assert!(
        !err.contains("sched-chat-1"),
        "the chat promise is never scanned by the CLI: {err}"
    );

    // Fail closed: the missed promise is marked, never fired late.
    let store = JsonScheduleStore::new(schedule_dir(&ws));
    let missed = store.load("sched-miss-1").unwrap().expect("record exists");
    assert_eq!(missed.status, ScheduledStatus::Missed);
    assert!(
        missed
            .result
            .clone()
            .unwrap_or_default()
            .contains("Re-schedule"),
        "the missed note names the way back: {:?}",
        missed.result
    );
    // The chat promise is untouched — still pending, still the chat's.
    let chat = store.load("sched-chat-1").unwrap().expect("record exists");
    assert_eq!(chat.status, ScheduledStatus::Pending);
    assert_eq!(chat.platform, "telegram");

    let _ = std::fs::remove_dir_all(&ws);
}

// ── M8 W6: cross-feature e2e ────────────────────────────────────────────────

#[tokio::test]
async fn resume_lands_a_swarm_with_the_chain_stamped() {
    let _guard = LOCK.lock().await;
    let ws = fresh_workspace("w6-resume-swarm");
    write_checkpoint(&ws, "sess-1", unix_now(), 2);

    // Four turns on one FIFO queue: the resumed parent spawns a child,
    // the child runs a gated command, then both answer.
    let mock = MockLlm::start(vec![
        tool_script("spawn_agent", r#"{"task":"run the child command"}"#),
        tool_call_script("echo child-side-effect"),
        vec![content_frame("child done")],
        vec![content_frame("Done.")],
    ])
    .await;
    let env_pairs = mock.env();
    let vars: Vec<(&str, &str)> = env_pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let env = set_env(&vars, &[]);
    let prior = set_workspace_env_to(&ws);
    let out = run_with(&["run", "--resume", "--allow-all", "--auto-approve"]).await;
    restore_workspace_env(prior);
    drop(env);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(err.contains("[session] resumed sess-1 (step 2)"), "{err}");
    assert_eq!(stdout(&out).trim(), "Done.");

    // The resumed task's swarm chains off the checkpoint's id: the child
    // runs under sess-1.1 and the swarm line names it (M8 W2 + W6).
    assert!(
        err.contains("[swarm] swarm: 1 sub-agent(s) (sess-1.1), 2 tool calls"),
        "{err}"
    );

    // The ledger stamps the child's executed call with the chain.
    let ledger = ws.join(".amparo/privacy/ledger.jsonl");
    let text = std::fs::read_to_string(&ledger).expect("ledger exists");
    let rows: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("one ledger row per line"))
        .collect();
    assert_eq!(rows.len(), 1, "the child's command is the only row: {text}");
    assert_eq!(rows[0]["task_id"], "sess-1.1");
    assert_eq!(rows[0]["parent_task_id"], "sess-1");
    assert_eq!(rows[0]["gate"], "human_approved");

    // The child's checkpoint names its parent — the delegation chain is
    // legible in the session files, not just the ledger.
    let child: Value = serde_json::from_str(
        &std::fs::read_to_string(ws.join(".amparo/sessions/cli/sess-1.1.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(child["status"], "complete");
    assert_eq!(child["parent_task_id"], "sess-1");
    // The resumed parent replaced its Running checkpoint with the
    // terminal one.
    let parent: Value = serde_json::from_str(
        &std::fs::read_to_string(ws.join(".amparo/sessions/cli/sess-1.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(parent["status"], "complete");
    let _ = std::fs::remove_dir_all(&ws);
}

// ── tui ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tui_piped_runs_a_task_and_renders_the_chain() {
    let _guard = LOCK.lock().await;
    let mock = MockLlm::start(vec![
        tool_script("read_file", r#"{"path":"README.md"}"#),
        vec![content_frame(" done.")],
    ])
    .await;
    let (env, prior) = mock_env(&mock).await;

    let out = run_with_stdin(&["tui", "--allow-all"], b"read the readme\n").await;

    restore_workspace_env(prior);
    drop(env);

    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    // The whole surface renders on stdout in piped mode: the banner, the
    // gate-chain gutter row for the executed tool, and the closing report
    // — all plain, zero escapes.
    assert!(text.contains("Greetings! My name is Amparo"), "{text}");
    assert!(text.contains("[wake] Amparo is awake."), "{text}");
    assert!(
        text.contains("Every action I take passes one chain"),
        "{text}"
    );
    assert!(text.contains("[key]"), "{text}");
    assert!(text.contains("[chain]"), "{text}");
    assert!(text.contains("◆▲§◉  read_file"), "{text}");
    assert!(text.contains("[report] complete —"), "{text}");
    assert!(
        !text.contains("\x1b["),
        "piped mode emits zero escapes: {text}"
    );
}

// ── Mock Guardrail Console ───────────────────────────────────────────────────

/// One scripted response: (method, path, status, json body). Requests
/// match by method+path, so the repeats the client performs per command
/// (one org resolution each) get the same response.
type ConsoleRow = (String, String, u16, Value);

/// A hand-rolled responder for the org-rules REST surface: the
/// `OrgPolicyClient` calls (method, path) pairs the test scripts, and
/// every request body is recorded so tests can assert what the TUI sent.
struct MockConsole {
    addr: std::net::SocketAddr,
    requests: Arc<tokio::sync::Mutex<Vec<(String, String, Value)>>>,
}

impl MockConsole {
    async fn start(script: Vec<ConsoleRow>) -> MockConsole {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock console");
        let addr = listener.local_addr().unwrap();
        let requests: Arc<tokio::sync::Mutex<Vec<(String, String, Value)>>> =
            Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let recorded = Arc::clone(&recorded);
                let script = script.clone();
                tokio::spawn(async move {
                    // Read head + Content-Length body (the MockPolicy
                    // pattern).
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 8192];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                        if buf.len() > 1_000_000 {
                            break;
                        }
                    }
                    let split = buf
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .map(|p| p + 4)
                        .unwrap_or(buf.len());
                    let head = String::from_utf8_lossy(&buf[..split]).to_string();
                    let content_length = head
                        .lines()
                        .find_map(|l| {
                            l.trim_start()
                                .to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    let mut body = buf[split..].to_vec();
                    while body.len() < content_length {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => body.extend_from_slice(&tmp[..n]),
                        }
                    }
                    let value = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
                    let mut req = head.lines().next().unwrap_or_default().split_whitespace();
                    let method = req.next().unwrap_or_default().to_string();
                    let path = req.next().unwrap_or_default().to_string();
                    recorded
                        .lock()
                        .await
                        .push((method.clone(), path.clone(), value));
                    let (status, json) = script
                        .iter()
                        .find(|(m, p, _, _)| m == &method && p == &path)
                        .map(|(_, _, s, b)| (*s, b.to_string()))
                        .unwrap_or_else(|| {
                            panic!(
                                "no scripted response for {method} {path} (body: {})",
                                String::from_utf8_lossy(&body)
                            )
                        });
                    let reason = match status {
                        200 | 201 => "OK",
                        400 => "Bad Request",
                        402 => "Payment Required",
                        404 => "Not Found",
                        409 => "Conflict",
                        _ => "Error",
                    };
                    let resp = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{json}",
                        json.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        MockConsole { addr, requests }
    }

    /// Base URL for `AMPARO_CONSOLE_POLICY_URL`.
    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Every recorded request: (method, path, body) in arrival order.
    async fn requests(&self) -> Vec<(String, String, Value)> {
        self.requests.lock().await.clone()
    }
}

// ── Mock engramd ─────────────────────────────────────────────────────────────

/// One scripted response keyed by request line prefix ("GET /health",
/// "POST /memories", …) — each path is requested once per test run (the
/// boot probe, then the command's own call).
struct MockEngramd {
    addr: std::net::SocketAddr,
    requests: Arc<tokio::sync::Mutex<Vec<(String, Value)>>>,
}

impl MockEngramd {
    async fn start(script: Vec<(String, u16, Value)>) -> MockEngramd {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock engramd");
        let addr = listener.local_addr().unwrap();
        let requests: Arc<tokio::sync::Mutex<Vec<(String, Value)>>> =
            Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let recorded = Arc::clone(&recorded);
                let script = script.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 8192];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                        if buf.len() > 1_000_000 {
                            break;
                        }
                    }
                    let split = buf
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .map(|p| p + 4)
                        .unwrap_or(buf.len());
                    let head = String::from_utf8_lossy(&buf[..split]).to_string();
                    let content_length = head
                        .lines()
                        .find_map(|l| {
                            l.trim_start()
                                .to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    let mut body = buf[split..].to_vec();
                    while body.len() < content_length {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => body.extend_from_slice(&tmp[..n]),
                        }
                    }
                    let value = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
                    let key = head
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .split_whitespace()
                        .take(2)
                        .collect::<Vec<_>>()
                        .join(" ");
                    recorded.lock().await.push((key.clone(), value));
                    let (status, json) = script
                        .iter()
                        .find(|(k, _, _)| *k == key)
                        .map(|(_, s, b)| (*s, b.to_string()))
                        .unwrap_or_else(|| {
                            panic!(
                                "no scripted response for {key} (body: {})",
                                String::from_utf8_lossy(&body)
                            )
                        });
                    let reason = if status == 200 { "OK" } else { "Not Found" };
                    let resp = format!(
                        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{json}",
                        json.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        MockEngramd { addr, requests }
    }

    /// Base URL for `AMPARO_ENGRAM_URL`.
    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Every recorded request: (request line, body) in arrival order.
    async fn requests(&self) -> Vec<(String, Value)> {
        self.requests.lock().await.clone()
    }
}

// ── tui /memory + /policy + ! ────────────────────────────────────────────────

#[tokio::test]
async fn tui_memory_add_and_search_roundtrip_through_the_engram_daemon() {
    let _guard = LOCK.lock().await;
    let mock = MockLlm::start(vec![]).await;
    let (env, prior) = mock_env(&mock).await;
    let engram = MockEngramd::start(vec![
        ("GET /health".to_string(), 200, json!({"status": "ok"})),
        (
            "POST /memories".to_string(),
            200,
            json!({"id": "mem-1", "content": "buy more coffee", "created_at": "2026-09-01T00:00:00Z"}),
        ),
        (
            "POST /memories/search".to_string(),
            200,
            json!({"results": [{"id": "mem-1", "content": "buy more coffee", "created_at": "2026-09-01T00:00:00Z"}]}),
        ),
    ])
    .await;
    let _mem_env = set_env(
        &[
            ("AMPARO_MEMORY_BACKEND", "engram"),
            ("AMPARO_ENGRAM_URL", &engram.url()),
        ],
        &[
            "AMPARO_ENGRAM_KEY",
            "AMPARO_POLICY_KEY",
            "AMPARO_CONSOLE_POLICY_URL",
        ],
    );

    let out = run_with_stdin(
        &["tui"],
        b"/memory add buy more coffee\n/memory search coffee\n",
    )
    .await;

    drop(_mem_env);
    restore_workspace_env(prior);
    drop(env);

    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(text.contains("Engram Vault"), "{text}");
    assert!(
        text.contains("[memory] stored mem-1"),
        "no session-only suffix on the vault store: {text}"
    );
    assert!(text.contains("[memory] 1 hit(s):"), "{text}");
    assert!(text.contains("buy more coffee   mem-1"), "{text}");
    // The human keystroke rides verbatim in the POST body.
    let posts: Vec<_> = engram
        .requests()
        .await
        .into_iter()
        .filter(|(k, _)| k == "POST /memories")
        .collect();
    assert_eq!(posts.len(), 1, "one capture: {posts:?}");
    assert_eq!(posts[0].1["content"], "buy more coffee");
    assert!(!text.contains("\x1b["), "zero escapes: {text}");
}

#[tokio::test]
async fn tui_memory_add_reports_a_filtered_capture() {
    let _guard = LOCK.lock().await;
    let mock = MockLlm::start(vec![]).await;
    let (env, prior) = mock_env(&mock).await;
    let _engram = MockEngramd::start(vec![
        ("GET /health".to_string(), 200, json!({"status": "ok"})),
        (
            "POST /memories".to_string(),
            200,
            json!({
                "id": "mem-9",
                "content": "noise",
                "created_at": "2026-09-01T00:00:00Z",
                "skipped": true,
                "skip_reason": "ignored source: interaction",
                "matched_id": null
            }),
        ),
    ])
    .await;
    let _mem_env = set_env(
        &[
            ("AMPARO_MEMORY_BACKEND", "engram"),
            ("AMPARO_ENGRAM_URL", &_engram.url()),
        ],
        &[
            "AMPARO_ENGRAM_KEY",
            "AMPARO_POLICY_KEY",
            "AMPARO_CONSOLE_POLICY_URL",
        ],
    );

    let out = run_with_stdin(&["tui"], b"/memory add noise\n").await;

    drop(_mem_env);
    restore_workspace_env(prior);
    drop(env);

    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(
        text.contains("[memory] filtered — ignored source: interaction"),
        "{text}"
    );
    assert!(!text.contains("\x1b["), "zero escapes: {text}");
}

#[tokio::test]
async fn tui_memory_add_builtin_reports_the_session_only_store() {
    let _guard = LOCK.lock().await;
    let mock = MockLlm::start(vec![]).await;
    let (env, prior) = mock_env(&mock).await;
    let _mem_env = set_env(
        &[],
        &[
            "AMPARO_MEMORY_BACKEND",
            "AMPARO_ENGRAM_URL",
            "AMPARO_ENGRAM_KEY",
            "AMPARO_POLICY_KEY",
            "AMPARO_CONSOLE_POLICY_URL",
        ],
    );

    let out = run_with_stdin(&["tui"], b"/memory add hello builtin\n/memory search hello\n").await;

    drop(_mem_env);
    restore_workspace_env(prior);
    drop(env);

    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(
        text.contains("[memory] stored mem-") && text.contains("built-in store (session-only)"),
        "{text}"
    );
    assert!(text.contains("[memory] 1 hit(s):"), "{text}");
    assert!(!text.contains("\x1b["), "zero escapes: {text}");
}

/// The console script the connected /policy tests share: one org, one
/// existing rule (r1), creates (r2), flips (r1), and the settings PUT.
fn policy_console_script() -> Vec<ConsoleRow> {
    vec![
        (
            "GET".to_string(),
            "/api/orgs/current".to_string(),
            200,
            json!({"org_id": "org-1", "name": "Test Org", "slug": "test", "enforce_mode": "audit"}),
        ),
        (
            "GET".to_string(),
            "/api/orgs/org-1/policies".to_string(),
            200,
            json!({"rules": [{
                "id": "r1", "org_id": "org-1", "tool_name": "read_file",
                "reason": "compliance hold", "enabled": true,
                "created_by": "u1", "created_at": "2026-09-01T00:00:00Z"
            }]}),
        ),
        (
            "POST".to_string(),
            "/api/orgs/org-1/policies".to_string(),
            201,
            json!({"rule": {
                "id": "r2", "org_id": "org-1", "tool_name": "shell",
                "reason": "paused", "enabled": true,
                "created_by": "u1", "created_at": "2026-09-01T00:00:00Z"
            }}),
        ),
        (
            "PUT".to_string(),
            "/api/orgs/org-1/policies/r1".to_string(),
            200,
            json!({"rule": {
                "id": "r1", "org_id": "org-1", "tool_name": "read_file",
                "reason": "compliance hold", "enabled": false,
                "created_by": "u1", "created_at": "2026-09-01T00:00:00Z"
            }}),
        ),
        (
            "PUT".to_string(),
            "/api/orgs/org-1/settings".to_string(),
            200,
            json!({"status": "ok"}),
        ),
    ]
}

#[tokio::test]
async fn tui_policy_writes_rules_through_the_console() {
    let _guard = LOCK.lock().await;
    let mock = MockLlm::start(vec![]).await;
    let (env, prior) = mock_env(&mock).await;
    let console = MockConsole::start(policy_console_script()).await;
    let policy = MockPolicy::start().await;
    let _pol_env = set_env(
        &[
            ("AMPARO_POLICY_KEY", "gk_test_org_key"),
            ("AMPARO_CONSOLE_POLICY_URL", &console.url()),
        ],
        &[],
    );

    let out = run_with_stdin(
        &["tui", "--policy-url", &policy.url()],
        b"/policy list\n/policy deny shell paused\n/policy toggle read_file\n/policy enforce\n",
    )
    .await;

    drop(_pol_env);
    restore_workspace_env(prior);
    drop(env);

    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(text.contains("[policy] 1 org rule(s) · org mode: audit"), "{text}");
    assert!(text.contains("read_file  enabled — compliance hold"), "{text}");
    assert!(text.contains("[policy] denied shell — rule r2 active"), "{text}");
    assert!(text.contains("[policy] read_file disabled"), "{text}");
    assert!(text.contains("[policy] org mode: enforce"), "{text}");
    // The writes carried the wire shapes: tool_name+reason on create,
    // enabled:false on toggle, enforce_mode on settings.
    let requests = console.requests().await;
    let post = requests
        .iter()
        .find(|(m, p, _)| m == "POST" && p == "/api/orgs/org-1/policies")
        .expect("deny POST");
    assert_eq!(post.2["tool_name"], "shell");
    assert_eq!(post.2["reason"], "paused");
    let flip = requests
        .iter()
        .find(|(m, p, _)| m == "PUT" && p == "/api/orgs/org-1/policies/r1")
        .expect("toggle PUT");
    assert_eq!(flip.2["enabled"], false);
    let mode = requests
        .iter()
        .find(|(m, p, _)| m == "PUT" && p == "/api/orgs/org-1/settings")
        .expect("settings PUT");
    assert_eq!(mode.2["enforce_mode"], "enforce");
    assert!(!text.contains("\x1b["), "zero escapes: {text}");
}

#[tokio::test]
async fn tui_policy_402_tier_text_rides_verbatim() {
    let _guard = LOCK.lock().await;
    let mock = MockLlm::start(vec![]).await;
    let (env, prior) = mock_env(&mock).await;
    let console = MockConsole::start(vec![
        (
            "GET".to_string(),
            "/api/orgs/current".to_string(),
            200,
            json!({"org_id": "org-1", "name": "Test Org", "slug": "test", "enforce_mode": "audit"}),
        ),
        (
            "PUT".to_string(),
            "/api/orgs/org-1/settings".to_string(),
            402,
            json!({"error": "Enforce mode requires the Pro plan or above. Your current plan is free."}),
        ),
    ])
    .await;
    let policy = MockPolicy::start().await;
    let _pol_env = set_env(
        &[
            ("AMPARO_POLICY_KEY", "gk_test_org_key"),
            ("AMPARO_CONSOLE_POLICY_URL", &console.url()),
        ],
        &[],
    );

    let out = run_with_stdin(&["tui", "--policy-url", &policy.url()], b"/policy enforce\n").await;

    drop(_pol_env);
    restore_workspace_env(prior);
    drop(env);

    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(
        text.contains("Enforce mode requires the Pro plan or above. Your current plan is free."),
        "{text}"
    );
    assert!(text.contains("[policy] HTTP 402"), "{text}");
    assert!(!text.contains("\x1b["), "zero escapes: {text}");
}

#[tokio::test]
async fn tui_policy_deny_conflict_points_at_toggle() {
    let _guard = LOCK.lock().await;
    let mock = MockLlm::start(vec![]).await;
    let (env, prior) = mock_env(&mock).await;
    let console = MockConsole::start(vec![
        (
            "GET".to_string(),
            "/api/orgs/current".to_string(),
            200,
            json!({"org_id": "org-1", "name": "Test Org", "slug": "test", "enforce_mode": "audit"}),
        ),
        (
            "GET".to_string(),
            "/api/orgs/org-1/policies".to_string(),
            200,
            json!({"rules": [{"id": "r9", "org_id": "org-1", "tool_name": "shell", "reason": "paused", "enabled": true}]}),
        ),
        (
            "POST".to_string(),
            "/api/orgs/org-1/policies".to_string(),
            409,
            json!({"error": "duplicate rule for shell"}),
        ),
    ])
    .await;
    let policy = MockPolicy::start().await;
    let _pol_env = set_env(
        &[
            ("AMPARO_POLICY_KEY", "gk_test_org_key"),
            ("AMPARO_CONSOLE_POLICY_URL", &console.url()),
        ],
        &[],
    );

    let out = run_with_stdin(&["tui", "--policy-url", &policy.url()], b"/policy deny shell x\n").await;

    drop(_pol_env);
    restore_workspace_env(prior);
    drop(env);

    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    // The console's error body rides verbatim (the client's contract) —
    // the guidance names the toggle path.
    assert!(
        text.contains("[policy] 'shell' already has a rule ({\"error\":\"duplicate rule for shell\"}) — /policy toggle shell"),
        "{text}"
    );
}

#[tokio::test]
async fn tui_policy_no_key_reports_not_connected_with_the_pairing_hint() {
    let _guard = LOCK.lock().await;
    let mock = MockLlm::start(vec![]).await;
    let (env, prior) = mock_env(&mock).await;
    let _pol_env = set_env(
        &[],
        &["AMPARO_POLICY_KEY", "AMPARO_CONSOLE_POLICY_URL"],
    );
    let policy = MockPolicy::start().await;

    let out = run_with_stdin(&["tui", "--policy-url", &policy.url()], b"/policy list\n").await;

    drop(_pol_env);
    restore_workspace_env(prior);
    drop(env);

    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(
        text.contains(
            "[policy] not connected — set AMPARO_POLICY_KEY (a gk_ org key) — `guardrail link` pairs this machine"
        ),
        "{text}"
    );
    assert!(!text.contains("\x1b["), "zero escapes: {text}");
}

#[tokio::test]
async fn tui_bang_runs_the_child_inline() {
    let _guard = LOCK.lock().await;
    let mock = MockLlm::start(vec![]).await;
    let (env, prior) = mock_env(&mock).await;

    let out = run_with_stdin(&["tui"], b"! echo hello-from-bang\n! exit 3\n").await;

    restore_workspace_env(prior);
    drop(env);

    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(text.contains("hello-from-bang"), "{text}");
    // The status line formats the std ExitStatus Display impl: Unix
    // renders "exit status: 3", Windows "exit code: 3".
    let status_needle = if cfg!(unix) {
        "[shell] exited exit status: 3"
    } else {
        "[shell] exited exit code: 3"
    };
    assert!(text.contains(status_needle), "{text}");
    assert!(!text.contains("\x1b["), "zero escapes: {text}");
}

// ── wizard ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn wizard_writes_0600_profile_and_a_run_reads_it() {
    let _guard = LOCK.lock().await;
    let mock = MockLlm::start(vec![vec![content_frame("Hello.")]]).await;
    // Strip every inference/policy/memory variable: the profile alone
    // must carry the follow-up run.
    let env = set_env(
        &[],
        &[
            "AMPARO_INFERENCE_URL",
            "AMPARO_INFERENCE_MODEL",
            "AMPARO_INFERENCE_KEY",
            "AMPARO_INFERENCE_PROVIDER",
            "AMPARO_POLICY_KEY",
            "AMPARO_CONSOLE_POLICY_URL",
            "AMPARO_MEMORY_BACKEND",
            "AMPARO_ENGRAM_URL",
            "AMPARO_ENGRAM_KEY",
        ],
    );
    let ws = fresh_workspace("wizard");
    let prior = set_workspace_env_to(&ws);

    // A fake bin dir first on PATH with a `guardrail` and an `engram`
    // executable — the delegation lines must report the sibling CLIs as
    // found (the real system PATH stays attached for the follow-up run).
    let fake_bin = ws.join("fake-bin");
    std::fs::create_dir_all(&fake_bin).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in ["guardrail", "engram"] {
            let f = fake_bin.join(name);
            std::fs::write(&f, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    #[cfg(not(unix))]
    {
        std::fs::write(fake_bin.join("guardrail.exe"), "x").unwrap();
        std::fs::write(fake_bin.join("engram.exe"), "x").unwrap();
    }
    let sep = if cfg!(windows) { ";" } else { ":" };
    let joined = format!(
        "{}{sep}{}",
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let joined: &'static str = Box::leak(joined.into_boxed_str());
    let path_env = set_env(&[("PATH", joined)], &[]);

    // Ten answers, one per prompt: workspace (empty = the env root),
    // url, model, then Enter for provider, key, policy url/key, a
    // console url, then Enter for memory url/key.
    let answers = format!("\n{}\nmock-model\n\n\n\n\nhttps://console.example\n\n\n", mock.url());
    let out = run_with_stdin(&["wizard"], answers.as_bytes()).await;

    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let profile_path = ws.join(".amparo").join("profile.json");
    assert!(
        text.contains(&format!(
            "[wizard] profile written to {} (mode 0600)",
            profile_path.display()
        )),
        "{text}"
    );
    // The summary and the boot-banner preview render the chain from the
    // answers — provider defaults to openai, policy to deny-all, memory
    // to the built-in store, and no key is ever echoed.
    assert!(
        text.contains("[chain] registry → trust ceiling (system_control)"),
        "{text}"
    );
    assert!(
        text.contains("policy (deny-all (no --policy-url or --allow-all))"),
        "{text}"
    );
    // The infer line shows the ledger's scheme://host[:port] shape — the
    // /v1 path is stripped by site_desc.
    let host = mock.url().trim_end_matches("/v1").to_string();
    assert!(
        text.contains(&format!("[infer] openai · mock-model · {host}")),
        "{text}"
    );
    assert!(text.contains("[memory] built-in store"), "{text}");
    assert!(text.contains("[wake] Amparo is awake."), "{text}");
    // Both sibling CLIs were found on the fake PATH, and the console
    // answer echoes back host-only in the summary.
    assert!(
        text.contains("[wizard] found the guardrail CLI — `guardrail link` pairs this machine with an org key"),
        "{text}"
    );
    assert!(
        text.contains("[wizard] found the engram CLI — `engram pair` pairs this machine with the memory daemon"),
        "{text}"
    );
    assert!(
        text.contains("[policy] console https://console.example — /policy commands write org rules there"),
        "{text}"
    );

    // The captured profile carries exactly the answers, owner-only.
    let profile: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&profile_path).unwrap()).unwrap();
    assert_eq!(profile["inference_url"], mock.url());
    assert_eq!(profile["inference_model"], "mock-model");
    assert!(profile["inference_provider"].is_null());
    assert!(profile["policy_url"].is_null());
    assert_eq!(profile["console_url"], "https://console.example");
    assert!(profile["memory_url"].is_null());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&profile_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the profile must be owner-only");
    }

    // A fresh process with the same stripped env resolves the inference
    // config from the profile alone.
    let out = run_with(&["run", "--allow-all", "say hello"]).await;
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("Hello."), "{}", stdout(&out));

    restore_workspace_env(prior);
    drop(path_env);
    drop(env);
    drop(mock);
    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn wizard_rejects_arguments() {
    let out = run_with(&["wizard", "extra"]).await;
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("takes no arguments (got extra)"),
        "{}",
        stderr(&out)
    );
}

#[tokio::test]
async fn wizard_at_eof_fails_instead_of_half_capturing() {
    let out = run_with(&["wizard"]).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("needs answers"), "{}", stderr(&out));
}

#[tokio::test]
async fn wizard_prints_install_hints_when_sibling_clis_are_missing() {
    let _guard = LOCK.lock().await;
    let ws = fresh_workspace("wizard-missing");
    let prior = set_workspace_env_to(&ws);
    // A PATH holding only an empty directory: neither sibling CLI can
    // resolve, so both delegation lines fall back to the install
    // one-liners. The wizard spawns nothing, so the bare PATH is safe.
    let empty_bin = ws.join("no-clis");
    std::fs::create_dir_all(&empty_bin).unwrap();
    let path_env: &'static str = Box::leak(empty_bin.display().to_string().into_boxed_str());
    let guard = set_env(&[("PATH", path_env)], &[]);

    // Ten empty answers — every step skipped, the delegation lines still
    // print before the first prompt of each step.
    let out = run_with_stdin(&["wizard"], b"\n\n\n\n\n\n\n\n\n\n").await;
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(
        text.contains(
            "[wizard] no guardrail CLI on PATH — install one with: curl -fsSL https://downloads.ellmstack.dev/install.sh | bash"
        ),
        "{text}"
    );
    assert!(
        text.contains(
            "[wizard] no engram CLI on PATH — install one with: curl -fsSL https://engram.ellmstack.dev/install.sh | bash"
        ),
        "{text}"
    );

    restore_workspace_env(prior);
    drop(guard);
    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn profile_policy_url_wires_the_engine_and_allow_all_suppresses_it() {
    let _guard = LOCK.lock().await;
    let mock = MockLlm::start(vec![
        tool_script("read_file", r#"{"path":"README.md"}"#),
        vec![content_frame("Done.")],
    ])
    .await;
    let policy = MockPolicy::start().await;
    let env = set_env(
        &[],
        &[
            "AMPARO_INFERENCE_URL",
            "AMPARO_INFERENCE_MODEL",
            "AMPARO_INFERENCE_KEY",
            "AMPARO_INFERENCE_PROVIDER",
            "AMPARO_POLICY_KEY",
            "AMPARO_MEMORY_BACKEND",
            "AMPARO_ENGRAM_URL",
            "AMPARO_ENGRAM_KEY",
        ],
    );
    let ws = fresh_workspace("wizard-policy");
    let prior = set_workspace_env_to(&ws);
    // A hand-written profile carrying the policy URL — the wizard's shape,
    // minus the interactive capture.
    std::fs::create_dir_all(ws.join(".amparo")).unwrap();
    std::fs::write(
        ws.join(".amparo").join("profile.json"),
        json!({
            "inference_url": mock.url(),
            "inference_model": "mock-model",
            "policy_url": policy.url(),
        })
        .to_string(),
    )
    .unwrap();

    // No --policy-url, no --allow-all: the profile URL is the engine, and
    // the banner marks the provenance. --auto-approve keeps the approval
    // gate out of the picture (stdin is closed) without touching policy.
    let out = run_with(&["run", "--auto-approve", "read the readme"]).await;
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "Done.");
    assert!(stderr(&out).contains("(profile)"), "{}", stderr(&out));
    // The engine saw exactly one check — the run carried no policy flag.
    assert_eq!(policy.bodies().await.len(), 1);

    // Under --allow-all the profile's policy URL is suppressed entirely:
    // a fresh env points the same workspace at a fresh mock, and no
    // second check reaches the engine.
    let mock2 = MockLlm::start(vec![vec![content_frame("Hello.")]]).await;
    let env2 = set_env(
        &[
            ("AMPARO_INFERENCE_URL", mock2.url().as_str()),
            ("AMPARO_INFERENCE_MODEL", "mock-model"),
        ],
        &[],
    );
    let out = run_with(&["run", "--allow-all", "say hello"]).await;
    drop(env2);
    drop(mock2);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("Hello."), "{}", stdout(&out));
    assert!(!stderr(&out).contains("(profile)"), "{}", stderr(&out));
    assert_eq!(policy.bodies().await.len(), 1);

    restore_workspace_env(prior);
    drop(env);
    drop(policy);
    drop(mock);
    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn code_help_exits_0() {
    let _guard = LOCK.lock().await;
    let out = run_with(&["code", "--help"]).await;
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(stdout(&out).contains("amparo code"), "{}", stdout(&out));
    assert!(stdout(&out).contains("amparo code [DIR]"), "{}", stdout(&out));
}

#[tokio::test]
async fn code_report_piped_zero_escapes() {
    let _guard = LOCK.lock().await;
    // A unique scratch tree under the shared workspace (the #174 lesson:
    // never share a fixture path across tests that may run in parallel).
    let tree = workspace().join("code-e2e-tree");
    std::fs::remove_dir_all(&tree).ok();
    std::fs::create_dir_all(tree.join("src")).expect("src dir");
    std::fs::write(tree.join("src/main.rs"), "fn main() {}\n").expect("main.rs");
    std::fs::write(tree.join("Cargo.toml"), "[package]\nname = \"x\"\n").expect("Cargo.toml");
    let prior = set_workspace_env();

    // stdin is null (piped), so the surface degrades to the report.
    let out = run_with(&["code", tree.to_str().expect("utf8 path")]).await;
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    let text = stdout(&out);
    assert!(text.contains("amparo code"), "{text}");
    assert!(text.contains("src"), "{text}");
    assert!(text.contains("main.rs"), "{text}");
    assert!(
        !text.contains("\x1b["),
        "piped report must be escape-free:\n{text}"
    );

    restore_workspace_env(prior);
    std::fs::remove_dir_all(&tree).ok();
}

#[tokio::test]
async fn code_piped_report_stays_an_honest_report() {
    let _guard = LOCK.lock().await;
    // A unique scratch tree under the shared workspace (the #174 lesson:
    // never share a fixture path across tests that may run in parallel).
    let tree = workspace().join("code-e2e-honest");
    std::fs::remove_dir_all(&tree).ok();
    std::fs::create_dir_all(tree.join("src")).expect("src dir");
    std::fs::write(tree.join("src/main.rs"), "fn main() {}\n").expect("main.rs");
    std::fs::write(tree.join("Cargo.toml"), "[package]\nname = \"x\"\n").expect("Cargo.toml");
    let prior = set_workspace_env();

    // Piped mode has no edit surface — the report is all it prints, and
    // none of the edit copy (diff lines, approval card, y/n status bar)
    // can leak into it. Editing needs a terminal; the report must not
    // pretend otherwise.
    let out = run_with(&["code", tree.to_str().expect("utf8 path")]).await;
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    let text = stdout(&out);
    assert!(text.contains("amparo code —"), "{text}");
    assert!(text.contains("main.rs"), "{text}");
    assert!(!text.contains("\x1b["), "zero escapes:\n{text}");
    assert!(!text.contains("approval"), "{text}");
    assert!(!text.contains("edit_file"), "{text}");
    assert!(!text.contains("patch_file"), "{text}");
    assert!(!text.contains("y apply"), "{text}");

    restore_workspace_env(prior);
    std::fs::remove_dir_all(&tree).ok();
}

#[tokio::test]
async fn code_missing_root_exits_1() {
    let _guard = LOCK.lock().await;
    let out = run_with(&["code", "/nonexistent-amparo-root-xyz"]).await;
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("amparo code"),
        "{}",
        stderr(&out)
    );
}

#[tokio::test]
async fn code_two_dirs_exit_2() {
    let _guard = LOCK.lock().await;
    let out = run_with(&["code", "a", "b"]).await;
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("one DIR at most"),
        "{}",
        stderr(&out)
    );
}
