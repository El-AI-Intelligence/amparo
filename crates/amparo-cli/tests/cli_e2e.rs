//! Real-process e2e for the shipped `amparo` binary.
//!
//! The inference endpoint is a hand-rolled mock LLM bound to a local port
//! (never 11434 — that would route to a real Ollama): `stream: true`
//! requests get scripted `data:` frames, the non-stream verification call
//! gets a fixed `VERIFIED` completion. Env-mutating tests hold [`LOCK`] and
//! restore what they touched — `AMPARO_WORKSPACE` is process-wide state and
//! the mock env must be visible to child processes only while they run.

use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

fn bin() -> String {
    // Set at test runtime by cargo (this cargo version does not expose it
    // at compile time via env!).
    std::env::var("CARGO_BIN_EXE_amparo").expect("cargo sets CARGO_BIN_EXE_amparo for tests")
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

/// Spawn `amparo` with a closed stdin and a hard timeout so a regression
/// can never hang the suite.
async fn run_with(args: &[&str]) -> Output {
    tokio::time::timeout(Duration::from_secs(30), async {
        tokio::process::Command::new(bin())
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .expect("spawn amparo")
    })
    .await
    .expect("amparo run timed out")
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
    tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
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
/// non-stream `VERIFIED` completion.
struct MockLlm {
    addr: std::net::SocketAddr,
}

impl MockLlm {
    async fn start(scripts: Vec<Script>) -> MockLlm {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let addr = listener.local_addr().unwrap();
        let scripts = Arc::new(tokio::sync::Mutex::new(scripts));
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let scripts = Arc::clone(&scripts);
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
        MockLlm { addr }
    }

    /// OpenAI-shaped base URL for `AMPARO_INFERENCE_URL`.
    fn url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    /// The env surface for the mock endpoint (openai provider is the default).
    fn env(&self) -> Vec<(String, String)> {
        vec![
            ("AMPARO_INFERENCE_URL".to_string(), self.url()),
            ("AMPARO_INFERENCE_MODEL".to_string(), "mock-model".to_string()),
        ]
    }
}

/// One SSE frame carrying a content delta.
fn content_frame(text: &str) -> Value {
    json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": "stop"}]})
}

/// A scripted turn that requests `run_command <command>`: the tool-call
/// frame plus the arguments fragment.
fn tool_call_script(command: &str) -> Script {
    vec![
        json!({
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "run_command", "arguments": ""}
                }]},
                "finish_reason": null
            }]
        }),
        json!({
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": [{
                    "index": 0,
                    "function": {"arguments": format!("{{\"command\":\"{command}\"}}")}
                }]},
                "finish_reason": "tool_calls"
            }]
        }),
    ]
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
    let out = std::process::Command::new(bin()).arg("version").output().unwrap();
    assert!(out.status.success());
    assert_eq!(
        stdout(&out).trim(),
        format!("amparo {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn unknown_subcommand_exits_2() {
    let out = std::process::Command::new(bin()).arg("frobnicate").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("unknown subcommand frobnicate"), "{}", stderr(&out));
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
    assert!(stderr(&out).contains("unknown flag --nonsense"), "{}", stderr(&out));
}

#[test]
fn run_missing_task_exits_2() {
    let out = std::process::Command::new(bin()).arg("run").output().unwrap();
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
    assert!(stderr(&out).contains("mutually exclusive"), "{}", stderr(&out));
}

#[test]
fn run_policy_conflict_exits_2() {
    let out = std::process::Command::new(bin())
        .args(["run", "--policy-url", "http://p.test", "--allow-all", "task"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("mutually exclusive"), "{}", stderr(&out));
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
    assert!(!result.isError, "list_dir should run under --allow-all: {text}");
    assert!(text.contains("marker.txt"), "the real tool ran against the real workspace: {text}");

    restore_workspace_env(prior);
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
    assert!(stderr(&out).contains("[report] complete"), "{}", stderr(&out));
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
    assert!(err.contains("[approval] granted"), "the human said yes: {err}");
    assert!(err.contains("[exec] run_command"), "the approved tool ran: {err}");
    assert_eq!(stdout(&out).trim(), "Done.");
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
    assert!(!err.contains("approve? [y/N]"), "no prompt may be printed: {err}");
    assert_eq!(stdout(&out).trim(), "Done.");
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
    assert!(err.contains("[growth] recording PII-stripped run records"), "{err}");

    let records = workspace().join(".amparo/notebook/records.jsonl");
    let text = std::fs::read_to_string(&records).expect("records file exists");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "exactly one record: {text}");
    assert!(!text.contains("user@example.com"), "raw email persisted: {text}");

    // One MemoryEntry per line; the entry's content is the RunRecord JSON.
    let entry: Value = serde_json::from_str(lines[0]).expect("line is JSON");
    let record: Value = serde_json::from_str(entry["content"].as_str().expect("content"))
        .expect("record JSON");
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
    assert!(!err.contains("[growth]"), "no growth line without the flag: {err}");
    assert!(
        !workspace().join(".amparo/notebook/records.jsonl").exists(),
        "no flag means no records file"
    );
}
