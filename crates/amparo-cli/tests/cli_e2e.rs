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
/// non-stream `VERIFIED` completion. Non-stream request bodies are recorded
/// so tests can inspect what the verification prompt actually carried.
struct MockLlm {
    addr: std::net::SocketAddr,
    /// Every non-stream (`complete`) request body, in arrival order.
    complete_requests: Arc<tokio::sync::Mutex<Vec<Value>>>,
}

impl MockLlm {
    async fn start(scripts: Vec<Script>) -> MockLlm {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let addr = listener.local_addr().unwrap();
        let scripts = Arc::new(tokio::sync::Mutex::new(scripts));
        let complete_requests: Arc<tokio::sync::Mutex<Vec<Value>>> =
            Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&complete_requests);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let scripts = Arc::clone(&scripts);
                let complete_requests = Arc::clone(&recorded);
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
    assert_eq!(requests.len(), 2, "one verification call per run: {requests:?}");
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
    assert!(stderr(&out).contains("missing subcommand"), "{}", stderr(&out));
    let out = run_with(&["skill", "frobnicate"]).await;
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    let out = run_with(&["skill", "add"]).await;
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("exactly one candidate file"), "{}", stderr(&out));
}

#[test]
fn skill_help_exits_0() {
    let out = std::process::Command::new(bin()).arg("skill").arg("--help").output().unwrap();
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
    assert!(!workspace().join(".amparo/skills/candidates/bad-skill.toml").exists());

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
    assert!(err.contains("no policy configured"), "deny-all reason surfaces: {err}");
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
    assert!(err.contains("[skills] adopted e2e-greet for tenant cli"), "{err}");
    let log = workspace().join(".amparo/skills/adopted.jsonl");
    let text = std::fs::read_to_string(&log).expect("audit log exists");
    let record: Value = serde_json::from_str(text.lines().next().expect("one line"))
        .expect("line is JSON");
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
    assert_eq!(adopted.status.code(), Some(0), "stderr: {}", stderr(&adopted));

    let out = run_with(&["skill", "list"]).await;
    let out_stdout = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(out_stdout.contains("e2e-greet — writes the e2e skill marker"), "{out_stdout}");

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
    assert_eq!(adopted.status.code(), Some(0), "stderr: {}", stderr(&adopted));

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
    assert!(err.contains("[growth] skills: 1 adopted for tenant cli"), "{err}");
    assert!(
        workspace().join("skill-marker.txt").exists(),
        "the skill's write_file step ran in the workspace"
    );
    assert_eq!(stdout(&out).trim(), "Skill done.");

    // The run record carries the use_skill call AND both expansion steps.
    let text = std::fs::read_to_string(workspace().join(".amparo/notebook/records.jsonl"))
        .expect("records exist");
    let record: Value = serde_json::from_str(
        serde_json::from_str::<Value>(text.lines().next().unwrap())
            .unwrap()["content"]
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
    assert_eq!(adopted.status.code(), Some(0), "stderr: {}", stderr(&adopted));

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
        let out = run_with(&["run", "--allow-all", "--auto-approve", "--growth", "list the workspace"]).await;
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
    assert!(out_stdout.contains("list-dir"), "suggested name: {out_stdout}");
    assert!(out_stdout.contains("origin = \"distilled\""), "{out_stdout}");
    assert!(out_stdout.contains("arguments = {}"), "inert skeleton: {out_stdout}");
    assert!(out_stdout.contains("3 run(s), 3 VERIFIED"), "evidence: {out_stdout}");
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
