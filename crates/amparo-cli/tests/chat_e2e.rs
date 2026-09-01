//! Real-process e2e for the `amparo chat` subcommand.
//!
//! The round trip runs against the shipped binary: `AMPARO_CHAT_TELEGRAM_BASE`
//! points the Telegram serve loop at a scripted mock Bot API, and
//! `AMPARO_INFERENCE_URL` points the agent at a hand-rolled mock LLM (same
//! scripted-SSE pattern as `cli_e2e.rs`). A message from an allowlisted user
//! starts a task, the task escalates `run_command` to the gate, the gate
//! sends an inline-keyboard approval, the button press is routed back, the
//! tool executes for real in the temp workspace, and the final answer is
//! delivered. All assertions poll the recorded request logs — request order
//! is a transport detail, not a contract.
//!
//! The CLI-surface tests (parsing, fail-closed env checks in wiring order —
//! bot token before inference env — and the exit-code contract: 0 help,
//! 2 flag/token problem, 1 serve failure) live beside the round trip.
//! Env-mutating tests hold [`LOCK`] and restore what they touched.

use serde_json::{Value, json};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

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

/// One shared workspace for the spawned chat binary (created once per test
/// process; `run_command` executes there).
fn workspace() -> &'static PathBuf {
    static WS: OnceLock<PathBuf> = OnceLock::new();
    WS.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(format!("amparo-cli-chat-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("workspace dir");
        dir
    })
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

/// Spawn `amparo chat` with a closed stdin and a hard timeout so a
/// regression can never hang the suite. The budget is deliberately
/// generous: round trips finish in under a second locally, but on a
/// loaded 2-core CI runner several concurrent `amparo chat` children
/// (each running a mock-LLM agent loop) pushed past 30s.
async fn run_with(args: &[&str]) -> Output {
    tokio::time::timeout(Duration::from_secs(120), async {
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
    .expect("amparo chat timed out")
}

/// Poll `cond` until it is true, or 120 seconds elapse. See the
/// [`run_with`] note: the budget absorbs loaded-runner contention, not
/// correctness — the conditions resolve in milliseconds when the runner
/// is idle.
async fn wait_until(cond: impl Fn() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    while tokio::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

// ── Mock LLM ─────────────────────────────────────────────────────────────────

/// One scripted `stream: true` response: the frames emitted before
/// `data: [DONE]`.
type Script = Vec<Value>;

/// A hand-rolled HTTP server: every connection gets one scripted SSE
/// response (consumed in order; the last script repeats) or the fixed
/// non-stream `VERIFIED` completion. Every request body is recorded so the
/// test can prove what the agent sent (e.g. a `tool_call_id` reference).
struct MockLlm {
    addr: SocketAddr,
    bodies: Arc<Mutex<Vec<String>>>,
}

impl MockLlm {
    async fn start(scripts: Vec<Script>) -> MockLlm {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let addr = listener.local_addr().unwrap();
        let scripts = Arc::new(tokio::sync::Mutex::new(scripts));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let log_bodies = Arc::clone(&bodies);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let scripts = Arc::clone(&scripts);
                let bodies = Arc::clone(&log_bodies);
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
                    let body = String::from_utf8_lossy(&body).to_string();
                    bodies.lock().unwrap().push(body.clone());

                    let streaming = body.contains("\"stream\":true");
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
        MockLlm { addr, bodies }
    }

    /// OpenAI-shaped base URL for `AMPARO_INFERENCE_URL`.
    fn url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    /// Whether some recorded request referenced `call_1` as a
    /// `tool_call_id` — the tool-result message only appears once the
    /// approved tool actually executed.
    fn saw_tool_result(&self) -> bool {
        self.bodies
            .lock()
            .unwrap()
            .iter()
            .any(|body| body.contains("tool_call_id") && body.contains("call_1"))
    }

    /// The prompt of every recorded non-stream (self-verification) request,
    /// in arrival order.
    fn verification_prompts(&self) -> Vec<String> {
        self.bodies
            .lock()
            .unwrap()
            .iter()
            .filter(|body| !body.contains("\"stream\":true"))
            .filter_map(|body| {
                let value: Value = serde_json::from_str(body).ok()?;
                Some(
                    value["messages"][0]["content"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                )
            })
            .collect()
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

// ── Mock Telegram Bot API ─────────────────────────────────────────────────────

/// One HTTP request the mock answered.
#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    path: String,
    body: String,
}

/// A scripted mock of the Telegram Bot API on 127.0.0.1:0. The
/// per-connection loop REPEATS until the client closes it — reqwest pools
/// connections, so a one-shot server would stall the second request on the
/// same pool.
struct MockTelegram {
    addr: SocketAddr,
    log: Arc<Mutex<Vec<Recorded>>>,
    updates: Arc<Mutex<VecDeque<Value>>>,
}

impl MockTelegram {
    async fn start() -> Arc<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let addr = listener.local_addr().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let updates = Arc::new(Mutex::new(VecDeque::new()));
        let next_message_id = Arc::new(AtomicI64::new(1000));
        tokio::spawn(serve_mock(
            listener,
            Arc::clone(&log),
            Arc::clone(&updates),
            Arc::clone(&next_message_id),
        ));
        Arc::new(Self { addr, log, updates })
    }

    /// The base URL `AMPARO_CHAT_TELEGRAM_BASE` points at.
    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Queue one update as the next `getUpdates` batch.
    fn push_update(&self, update: Value) {
        self.updates.lock().unwrap().push_back(update);
    }

    /// Every recorded request, in arrival order.
    fn log(&self) -> Vec<Recorded> {
        self.log.lock().unwrap().clone()
    }
}

/// Accept connections and answer each on its own task.
async fn serve_mock(
    listener: tokio::net::TcpListener,
    log: Arc<Mutex<Vec<Recorded>>>,
    updates: Arc<Mutex<VecDeque<Value>>>,
    next_message_id: Arc<AtomicI64>,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else { return };
        tokio::spawn(serve_connection(
            stream,
            Arc::clone(&log),
            Arc::clone(&updates),
            Arc::clone(&next_message_id),
        ));
    }
}

/// Answer requests on one connection until the client closes it.
async fn serve_connection(
    stream: tokio::net::TcpStream,
    log: Arc<Mutex<Vec<Recorded>>>,
    updates: Arc<Mutex<VecDeque<Value>>>,
    next_message_id: Arc<AtomicI64>,
) -> std::io::Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    loop {
        let Some((request_line, body)) = read_request(&mut reader).await? else {
            return Ok(()); // clean close — reqwest ended the connection
        };
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("").to_string();
        log.lock().unwrap().push(Recorded { method, path: path.clone(), body: body.clone() });

        let endpoint = path
            .rsplit_once('/')
            .map(|(_, rest)| rest.split('?').next().unwrap_or(""))
            .unwrap_or("");
        let answer = match endpoint {
            "getUpdates" => {
                let keyboard_sent = log.lock().unwrap().iter().any(|r| {
                    r.path.contains("/sendMessage") && body_contains(&r.body, "inline_keyboard")
                });
                let mut updates = updates.lock().unwrap();
                // The message batch is delivered immediately; the callback
                // batch is held until the keyboard approval is on screen, so
                // the press can never land before the gate is registered.
                if keyboard_sent || updates.front().map_or(true, |u| u.get("message").is_some()) {
                    match updates.pop_front() {
                        Some(update) => json!({"ok": true, "result": [update]}),
                        None => json!({"ok": true, "result": []}),
                    }
                } else {
                    json!({"ok": true, "result": []})
                }
            }
            "sendMessage" => json!({
                "ok": true,
                "result": {"message_id": next_message_id.fetch_add(1, Ordering::SeqCst)}
            }),
            "editMessageText" => json!({"ok": true, "result": true}),
            "answerCallbackQuery" => json!({"ok": true, "result": true}),
            other => json!({"ok": false, "error_code": 404, "description": format!("unknown method {other}")}),
        };
        write_response(&mut writer, &answer.to_string()).await?;
    }
}

/// Read one request: head (up to `\r\n\r\n`) plus the `Content-Length`
/// body. `Ok(None)` = clean close between requests.
async fn read_request(
    reader: &mut (impl AsyncRead + Unpin),
) -> std::io::Result<Option<(String, String)>> {
    let mut bytes: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            if bytes.is_empty() {
                return Ok(None);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed mid-request-head",
            ));
        }
        bytes.extend_from_slice(&chunk[..n]);
        if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&bytes[..head_end]).to_string();
    let mut body = bytes[head_end..].to_vec();
    let content_length = head
        .to_ascii_lowercase()
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    while body.len() < content_length {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed mid-body",
            ));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);
    let request_line = head.lines().next().unwrap_or("").to_string();
    Ok(Some((request_line, String::from_utf8_lossy(&body).to_string())))
}

/// Whether a form-url-encoded request body contains `needle` after
/// decoding — request bodies arrive `%XX`-encoded, so a plain
/// `contains("approve:call_1")` would never match.
fn body_contains(body: &str, needle: &str) -> bool {
    decode_form(body).contains(needle)
}

/// Decode a form-url-encoded body (`%XX` escapes and `+` spaces).
fn decode_form(body: &str) -> String {
    let bytes = body.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(body.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                out.push(high * 16 + low);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// A single hex digit value, or `None` for anything else.
fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Write a minimal JSON 200 response.
async fn write_response(
    writer: &mut (impl AsyncWrite + Unpin),
    body: &str,
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: keep-alive\r\n\r\n",
        body.len()
    );
    writer.write_all(head.as_bytes()).await?;
    writer.write_all(body.as_bytes()).await?;
    writer.flush().await
}

/// An update carrying a text message from `user` in `chat`.
fn message_update(update_id: i64, user_id: i64, chat_id: i64, text: &str) -> Value {
    json!({
        "update_id": update_id,
        "message": {
            "message_id": update_id + 400,
            "from": {"id": user_id, "is_bot": false, "first_name": "Test"},
            "chat": {"id": chat_id, "type": "private"},
            "text": text,
        }
    })
}

/// An update carrying a callback query with button payload `data`.
fn callback_update(update_id: i64, user_id: i64, chat_id: i64, data: &str) -> Value {
    json!({
        "update_id": update_id,
        "callback_query": {
            "id": format!("cb_{update_id}"),
            "from": {"id": user_id, "is_bot": false, "first_name": "Test"},
            "message": {
                "message_id": update_id + 500,
                "chat": {"id": chat_id, "type": "private"},
                "text": "Approval needed",
            },
            "data": data,
        }
    })
}

// ── CLI surface ──────────────────────────────────────────────────────────────

#[test]
fn chat_without_platform_exits_2() {
    let out = std::process::Command::new(bin()).arg("chat").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("requires a platform"), "{}", stderr(&out));
}

#[test]
fn chat_unknown_platform_exits_2() {
    let out = std::process::Command::new(bin())
        .args(["chat", "unknownplatform"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("unknown platform 'unknownplatform'"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn chat_unknown_flag_exits_2() {
    let out = std::process::Command::new(bin())
        .args(["chat", "telegram", "--nonsense"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("unknown flag --nonsense"), "{}", stderr(&out));
}

#[test]
fn chat_conflicting_modes_exit_2() {
    let out = std::process::Command::new(bin())
        .args(["chat", "telegram", "--allow-all", "--policy-url", "http://p.test"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("mutually exclusive"), "{}", stderr(&out));
}

#[test]
fn chat_help_exits_0() {
    let out = std::process::Command::new(bin())
        .args(["chat", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = stdout(&out);
    assert!(stdout.contains("telegram"), "{}", stdout);
    assert!(stdout.contains("AMPARO_CHAT_TELEGRAM_TOKEN"), "{}", stdout);
}

#[test]
fn main_help_lists_chat() {
    let out = std::process::Command::new(bin()).arg("--help").output().unwrap();
    assert!(out.status.success());
    assert!(
        stdout(&out).contains("amparo chat telegram|discord|slack [FLAGS]"),
        "{}",
        stdout(&out)
    );
}

// ── Fail-closed env checks in wiring order ───────────────────────────────────

#[tokio::test]
async fn chat_missing_token_exits_2() {
    let _guard = LOCK.lock().await;
    let env = set_env(&[], &["AMPARO_CHAT_TELEGRAM_TOKEN"]);
    let out = run_with(&["chat", "telegram"]).await;
    drop(env);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("AMPARO_CHAT_TELEGRAM_TOKEN"),
        "the fail-closed message names the missing token: {}",
        stderr(&out)
    );
}

#[tokio::test]
async fn chat_serve_fails_closed_without_inference_env() {
    let _guard = LOCK.lock().await;
    // The token check runs before the inference wiring, so a token alone is
    // not enough: serve() must still fail closed on the inference env, and
    // that is a serve failure (exit 1), not a flag problem (exit 2).
    let env = set_env(
        &[("AMPARO_CHAT_TELEGRAM_TOKEN", "test-token")],
        &["AMPARO_INFERENCE_URL", "AMPARO_INFERENCE_MODEL"],
    );
    let out = run_with(&["chat", "telegram", "--allow-all"]).await;
    drop(env);
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("AMPARO_INFERENCE_URL"),
        "fail-closed message names the missing env: {}",
        stderr(&out)
    );
}

// ── Full round trip ──────────────────────────────────────────────────────────

#[tokio::test]
async fn chat_telegram_roundtrip_message_approval_tool_answer() {
    let _guard = LOCK.lock().await;

    // The agent: one tool-calling turn (run_command "echo hi", id call_1)
    // then a final answer. The tool only ever runs if the gate approves.
    let llm = MockLlm::start(vec![
        tool_call_script("echo hi"),
        vec![content_frame("Done.")],
    ])
    .await;
    // The bot API: a message from the allowlisted user, then the Approve
    // press, then nothing.
    let telegram = MockTelegram::start().await;
    telegram.push_update(message_update(101, 111, 111, "do it"));
    telegram.push_update(callback_update(102, 111, 111, "approve:call_1"));

    let ws = workspace().display().to_string();
    let base = telegram.url();
    let llm_url = llm.url();
    let env = set_env(
        &[
            ("AMPARO_CHAT_TELEGRAM_TOKEN", "test-token"),
            ("AMPARO_CHAT_ALLOWLIST", "111"),
            ("AMPARO_CHAT_TELEGRAM_BASE", base.as_str()),
            ("AMPARO_INFERENCE_URL", llm_url.as_str()),
            ("AMPARO_INFERENCE_MODEL", "mock-model"),
            ("AMPARO_WORKSPACE", ws.as_str()),
        ],
        &[],
    );

    // --allow-all skips the policy gate, but approval still fires: the
    // external-effector run_command tier escalates to the inline buttons.
    let mut child = tokio::process::Command::new(bin())
        .args(["chat", "telegram", "--allow-all"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn amparo chat");

    // The round trip is complete once the tool result reached the LLM AND a
    // sendMessage carrying the final answer was recorded. Order is not
    // asserted — the log is polled until both appear.
    let done = wait_until(|| {
        telegram.log().iter().any(|r| {
            r.path.contains("/sendMessage") && body_contains(&r.body, "Done.")
        }) && llm.saw_tool_result()
    })
    .await;

    let _ = child.kill().await;
    let output = child.wait_with_output().await.expect("wait for amparo chat");
    drop(env);

    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        done,
        "the round trip never completed within 120s; child stderr: {err}"
    );

    let log = telegram.log();
    assert!(
        log.iter().any(|r| r.method == "POST"
            && r.path.contains("/sendMessage")
            && body_contains(&r.body, "inline_keyboard")
            && body_contains(&r.body, "approve:call_1")),
        "the approval message carries the inline Approve button: {log:#?}"
    );
    assert!(
        log.iter().any(|r| r.path.contains("/answerCallbackQuery")),
        "the button press was answered so the client spinner stops: {log:#?}"
    );
    assert!(
        log.iter().any(|r| r.path.contains("/editMessageText")),
        "the gate edited the approval message with the outcome: {log:#?}"
    );
    assert!(
        llm.saw_tool_result(),
        "the LLM saw the tool_result for call_1 — the approved tool ran for real"
    );
}

// ── Growth notebook (M6a + M6b) ──────────────────────────────────────────────

/// Whether the notebook file holds a record tagged with `tenant`.
///
/// Each line is a memory entry whose `content` field is the escaped record
/// JSON — parsed twice, like any consumer of the store.
fn record_landed(path: &std::path::Path, tenant: &str) -> bool {
    let Ok(raw) = std::fs::read_to_string(path) else { return false };
    raw.lines().any(|line| {
        let Ok(outer) = serde_json::from_str::<Value>(line) else { return false };
        let Some(content) = outer.get("content").and_then(|c| c.as_str()) else {
            return false;
        };
        let Ok(inner) = serde_json::from_str::<Value>(content) else { return false };
        inner.get("tenant_id").and_then(|t| t.as_str()) == Some(tenant)
    })
}

#[tokio::test]
async fn chat_telegram_growth_writes_run_record() {
    let _guard = LOCK.lock().await;

    // A text-only turn (no tools, no approval): the record is written when
    // the task completes. The mock repeats its last script, so one script
    // covers the whole task.
    let llm = MockLlm::start(vec![vec![content_frame("Done.")]]).await;
    let telegram = MockTelegram::start().await;
    telegram.push_update(message_update(
        701,
        111,
        111,
        "remember my email user@example.com please",
    ));

    let ws = fresh_workspace("growth").display().to_string();
    let base = telegram.url();
    let llm_url = llm.url();
    let env = set_env(
        &[
            ("AMPARO_CHAT_TELEGRAM_TOKEN", "test-token"),
            ("AMPARO_CHAT_ALLOWLIST", "111"),
            ("AMPARO_CHAT_TELEGRAM_BASE", base.as_str()),
            ("AMPARO_INFERENCE_URL", llm_url.as_str()),
            ("AMPARO_INFERENCE_MODEL", "mock-model"),
            ("AMPARO_WORKSPACE", ws.as_str()),
        ],
        &[],
    );

    let mut child = spawn_serve(&["chat", "telegram", "--allow-all", "--growth"]);

    // Poll the record FILE itself — not a sendMessage — so the assert can
    // never race the spawned record write against the child being killed.
    let records_path = PathBuf::from(&ws).join(".amparo/notebook/records.jsonl");
    let written = wait_until(|| record_landed(&records_path, "telegram:111")).await;

    let _ = child.kill().await;
    let output = child.wait_with_output().await.expect("wait for amparo chat");
    drop(env);

    let err = String::from_utf8_lossy(&output.stderr);
    assert!(written, "no tenant-tagged record within 120s; child stderr: {err}");
    assert!(err.contains("[growth]"), "startup reports the notebook: {err}");

    let raw = std::fs::read_to_string(&records_path).expect("records file exists");
    let record = raw
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|outer| outer.get("content")?.as_str().map(str::to_string))
        .filter_map(|content| serde_json::from_str::<Value>(&content).ok())
        .find(|inner| inner.get("tenant_id").and_then(|t| t.as_str()) == Some("telegram:111"))
        .expect("the tenant-tagged record");
    let task_text = record.get("task_text").and_then(|t| t.as_str()).expect("task text");
    assert!(task_text.contains("[EMAIL_1]"), "stripped placeholder kept: {task_text}");
    assert!(!task_text.contains("user@example.com"), "raw email stripped: {task_text}");
}

#[tokio::test]
async fn chat_telegram_growth_retrieves_prior_cases_into_verification() {
    let _guard = LOCK.lock().await;

    // Two text-only tasks from the same user in different chats (the busy
    // claim is per chat, so the second task never races the first). Task 1
    // seeds the notebook; task 2's verification prompt must carry the case.
    let llm = MockLlm::start(vec![vec![content_frame("Done.")]]).await;
    let telegram = MockTelegram::start().await;
    telegram.push_update(message_update(801, 111, 111, "deploy the staging site"));

    let ws = fresh_workspace("growth-retrieval").display().to_string();
    let base = telegram.url();
    let llm_url = llm.url();
    let env = set_env(
        &[
            ("AMPARO_CHAT_TELEGRAM_TOKEN", "test-token"),
            ("AMPARO_CHAT_ALLOWLIST", "111"),
            ("AMPARO_CHAT_TELEGRAM_BASE", base.as_str()),
            ("AMPARO_INFERENCE_URL", llm_url.as_str()),
            ("AMPARO_INFERENCE_MODEL", "mock-model"),
            ("AMPARO_WORKSPACE", ws.as_str()),
        ],
        &[],
    );

    let mut child = spawn_serve(&["chat", "telegram", "--allow-all", "--growth"]);

    // Task 1's record must land before task 2 starts — its verification
    // precedes the record, so the prompt order below is deterministic.
    let records_path = PathBuf::from(&ws).join(".amparo/notebook/records.jsonl");
    let first = wait_until(|| record_landed(&records_path, "telegram:111")).await;
    assert!(first, "task 1 record never landed");

    telegram.push_update(message_update(802, 111, 222, "deploy the staging site"));
    let verified = wait_until(|| llm.verification_prompts().len() >= 2).await;

    let _ = child.kill().await;
    let output = child.wait_with_output().await.expect("wait for amparo chat");
    drop(env);

    let err = String::from_utf8_lossy(&output.stderr);
    assert!(verified, "second verification never arrived; child stderr: {err}");

    let prompts = llm.verification_prompts();
    assert!(
        !prompts[0].contains("Prior cases"),
        "an empty notebook leaves the first verification unchanged: {}",
        prompts[0]
    );
    assert!(
        prompts[1].contains("Prior cases in this tenant resembling the current task:"),
        "the second verification carries the evidence section: {}",
        prompts[1]
    );
    assert!(
        prompts[1].contains("deploy the staging site"),
        "the evidence names the prior task: {}",
        prompts[1]
    );
}

#[tokio::test]
async fn chat_telegram_growth_promotes_records_into_the_hot_layer() {
    let _guard = LOCK.lock().await;

    // Two text-only tasks from the same user in different chats. Task 1
    // writes the cold record; task 2's start promotes the tail into the
    // hot layer, whose row must carry the cold id.
    let llm = MockLlm::start(vec![vec![content_frame("Done.")]]).await;
    let telegram = MockTelegram::start().await;
    telegram.push_update(message_update(901, 111, 111, "first task"));

    let ws = fresh_workspace("hot-layer").display().to_string();
    let base = telegram.url();
    let llm_url = llm.url();
    let env = set_env(
        &[
            ("AMPARO_CHAT_TELEGRAM_TOKEN", "test-token"),
            ("AMPARO_CHAT_ALLOWLIST", "111"),
            ("AMPARO_CHAT_TELEGRAM_BASE", base.as_str()),
            ("AMPARO_INFERENCE_URL", llm_url.as_str()),
            ("AMPARO_INFERENCE_MODEL", "mock-model"),
            ("AMPARO_WORKSPACE", ws.as_str()),
        ],
        &[],
    );

    let mut child = spawn_serve(&["chat", "telegram", "--allow-all", "--growth"]);

    // Task 1's cold record must land before task 2 starts — the promotion
    // happens at task 2's start.
    let records_path = PathBuf::from(&ws).join(".amparo/notebook/records.jsonl");
    let first = wait_until(|| record_landed(&records_path, "telegram:111")).await;
    assert!(first, "task 1 record never landed");

    telegram.push_update(message_update(902, 111, 222, "second task"));
    let hot_path = PathBuf::from(&ws).join(".amparo/notebook/hot.jsonl");
    let promoted = wait_until(|| record_landed(&hot_path, "telegram:111")).await;

    let _ = child.kill().await;
    let output = child.wait_with_output().await.expect("wait for amparo chat");
    drop(env);

    let err = String::from_utf8_lossy(&output.stderr);
    assert!(promoted, "no hot row within 120s; child stderr: {err}");

    // The hot row keeps the cold row's id (id stability across layers).
    let id_of = |path: &std::path::Path| -> String {
        let raw = std::fs::read_to_string(path).expect("store file");
        let outer: Value =
            serde_json::from_str(raw.lines().next().expect("one row")).expect("entry JSON");
        outer["id"].as_str().expect("entry id").to_string()
    };
    let hot_raw = std::fs::read_to_string(&hot_path).expect("hot layer");
    assert_eq!(hot_raw.lines().count(), 1, "one hot row: {hot_raw}");
    assert_eq!(
        id_of(&hot_path),
        id_of(&records_path),
        "the hot row keeps the cold id"
    );
}

// ── Chat config (M5 tenant directory) ───────────────────────────────────────

/// Spawn `amparo chat telegram` as a serve process (closed stdin, piped
/// stdio) — the round-trip spawn pattern, shared by the config tests.
fn spawn_serve(args: &[&str]) -> tokio::process::Child {
    tokio::process::Command::new(bin())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn amparo chat")
}

/// Write a tenant-directory config to a uniquely named file under the
/// system temp dir (never the repo tree). The caller removes the file.
fn write_chat_config(name: &str, body: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("amparo-cli-chat-config-{name}.toml"));
    std::fs::write(&path, body).expect("write chat config");
    path
}

/// A uniquely named workspace root for one test's per-user directories
/// (fresh: any stale dir from a crashed run is cleared first).
fn fresh_workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("amparo-cli-chat-cfg-ws-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create workspace root");
    dir
}

#[tokio::test]
async fn chat_config_flag_without_value_exits_2() {
    let _guard = LOCK.lock().await;
    let out = run_with(&["chat", "telegram", "--chat-config"]).await;
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("requires a path"), "{}", stderr(&out));
}

#[tokio::test]
async fn chat_config_missing_file_exits_2() {
    let _guard = LOCK.lock().await;
    // Token and inference env are valid, so the config load is the only
    // possible failure — and it must be a configuration problem (exit 2),
    // not a serve failure (exit 1).
    let env = set_env(
        &[
            ("AMPARO_CHAT_TELEGRAM_TOKEN", "test-token"),
            ("AMPARO_INFERENCE_URL", "http://127.0.0.1:9/v1"),
            ("AMPARO_INFERENCE_MODEL", "mock-model"),
        ],
        &["AMPARO_CHAT_CONFIG", "AMPARO_CHAT_ALLOWLIST"],
    );
    let out = run_with(&[
        "chat", "telegram", "--chat-config", "/nonexistent/amparo-tenants.toml",
    ])
    .await;
    drop(env);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("chat config"), "{}", stderr(&out));
}

#[tokio::test]
async fn chat_config_flag_wins_over_env() {
    let _guard = LOCK.lock().await;
    // The env config allows telegram:111; the flag config is empty. If the
    // env path were used, 111 would get a task — the refusal proves the
    // flag won, and the empty-config warning on stderr says so explicitly.
    let env_cfg = write_chat_config("flag-wins-env", "[users.\"telegram:111\"]\n");
    let flag_cfg = write_chat_config("flag-wins", "[users]\n");
    let llm = MockLlm::start(vec![]).await;
    let telegram = MockTelegram::start().await;
    telegram.push_update(message_update(201, 111, 111, "do it"));

    let base = telegram.url();
    let llm_url = llm.url();
    let env_cfg_str = env_cfg.to_str().unwrap();
    let flag_cfg_str = flag_cfg.to_str().unwrap();
    let env = set_env(
        &[
            ("AMPARO_CHAT_TELEGRAM_TOKEN", "test-token"),
            ("AMPARO_CHAT_ALLOWLIST", "111"),
            ("AMPARO_CHAT_TELEGRAM_BASE", base.as_str()),
            ("AMPARO_INFERENCE_URL", llm_url.as_str()),
            ("AMPARO_INFERENCE_MODEL", "mock-model"),
            ("AMPARO_CHAT_CONFIG", env_cfg_str),
        ],
        &["AMPARO_WORKSPACE"],
    );

    let mut child =
        spawn_serve(&["chat", "telegram", "--chat-config", flag_cfg_str]);

    let refused = wait_until(|| {
        telegram.log().iter().any(|r| {
            r.path.contains("/sendMessage") && body_contains(&r.body, "not authorized")
        })
    })
    .await;
    let _ = child.kill().await;
    let output = child.wait_with_output().await.expect("wait for amparo chat");
    drop(env);
    std::fs::remove_file(&env_cfg).ok();
    std::fs::remove_file(&flag_cfg).ok();

    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        refused,
        "111 must be refused by the empty flag config; child stderr: {err}"
    );
    assert!(
        llm.bodies.lock().unwrap().is_empty(),
        "a refused user never reaches the LLM"
    );
    assert!(
        err.contains("no users in chat config"),
        "the empty-config warning proves the flag path loaded: {err}"
    );
}

#[tokio::test]
async fn chat_config_directory_roundtrip_per_user_workspace() {
    let _guard = LOCK.lock().await;
    let cfg = write_chat_config("per-user-ws", "[users.\"telegram:111\"]\n");
    let ws = fresh_workspace("per-user-ws");
    // The probe that prints the working directory: `pwd` on Unix shells,
    // `cd` (no arguments) on `cmd` — the run_command tool runs `bash -c`
    // on Unix and `cmd /C` on Windows.
    let probe = if cfg!(windows) { "cd" } else { "pwd" };
    let llm = MockLlm::start(vec![tool_call_script(probe), vec![content_frame("Done.")]]).await;
    let telegram = MockTelegram::start().await;
    telegram.push_update(message_update(301, 111, 111, "do it"));
    telegram.push_update(callback_update(302, 111, 111, "approve:call_1"));

    let base = telegram.url();
    let llm_url = llm.url();
    let ws_str = ws.to_str().unwrap();
    let cfg_str = cfg.to_str().unwrap();
    // AMPARO_CHAT_ALLOWLIST is set on purpose: it must be ignored while a
    // chat config is set, and the warning on stderr proves that.
    let env = set_env(
        &[
            ("AMPARO_CHAT_TELEGRAM_TOKEN", "test-token"),
            ("AMPARO_CHAT_ALLOWLIST", "111"),
            ("AMPARO_CHAT_TELEGRAM_BASE", base.as_str()),
            ("AMPARO_INFERENCE_URL", llm_url.as_str()),
            ("AMPARO_INFERENCE_MODEL", "mock-model"),
            ("AMPARO_WORKSPACE", ws_str),
        ],
        &["AMPARO_CHAT_CONFIG"],
    );

    let mut child = spawn_serve(&["chat", "telegram", "--allow-all", "--chat-config", cfg_str]);

    let done = wait_until(|| {
        telegram.log().iter().any(|r| {
            r.path.contains("/sendMessage") && body_contains(&r.body, "Done.")
        }) && llm.saw_tool_result()
    })
    .await;

    let _ = child.kill().await;
    let output = child.wait_with_output().await.expect("wait for amparo chat");
    drop(env);

    let per_user = ws.join("users/telegram-111");
    assert!(
        done,
        "the per-user workspace round trip never completed; child stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        per_user.is_dir(),
        "the per-user workspace exists on disk: {per_user:?}"
    );
    // On failure the bodies alone are not enough to see where the flow
    // broke (e.g. the approval gate auto-denying) — the child's stderr
    // and the mock's request log pin the exact step.
    let child_stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let telegram_requests = format!("{:?}", telegram.log());
    let bodies = llm.bodies.lock().unwrap();
    assert!(
        bodies.iter().any(|b| b.contains(&per_user.to_string_lossy().to_string())),
        "the run_command result echoed the per-user cwd: {bodies:?}\nchild stderr: {child_stderr}\ntelegram requests: {telegram_requests}"
    );
    assert!(
        bodies.iter().any(|b| b.contains("\\\"exit_code\\\":0")),
        "the tool truly ran (success result present): {bodies:?}\nchild stderr: {child_stderr}\ntelegram requests: {telegram_requests}"
    );
    drop(bodies);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("AMPARO_CHAT_ALLOWLIST is ignored"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    std::fs::remove_file(&cfg).ok();
    std::fs::remove_dir_all(&ws).ok();
}

#[tokio::test]
async fn chat_config_wrong_user_press_is_toasted_and_requester_still_decides() {
    let _guard = LOCK.lock().await;
    let cfg = write_chat_config("wrong-user", "[users.\"telegram:111\"]\n");
    let ws = fresh_workspace("wrong-user");
    let llm = MockLlm::start(vec![tool_call_script("echo hi"), vec![content_frame("Done.")]]).await;
    let telegram = MockTelegram::start().await;
    telegram.push_update(message_update(401, 111, 111, "do it"));
    // FIFO: 222's press is answered before 111's own approve — the toast
    // must land, and the approval must stay pending for the requester.
    telegram.push_update(callback_update(402, 222, 111, "approve:call_1"));
    telegram.push_update(callback_update(403, 111, 111, "approve:call_1"));

    let base = telegram.url();
    let llm_url = llm.url();
    let ws_str = ws.to_str().unwrap();
    let cfg_str = cfg.to_str().unwrap();
    let env = set_env(
        &[
            ("AMPARO_CHAT_TELEGRAM_TOKEN", "test-token"),
            ("AMPARO_CHAT_TELEGRAM_BASE", base.as_str()),
            ("AMPARO_INFERENCE_URL", llm_url.as_str()),
            ("AMPARO_INFERENCE_MODEL", "mock-model"),
            ("AMPARO_WORKSPACE", ws_str),
        ],
        &["AMPARO_CHAT_CONFIG", "AMPARO_CHAT_ALLOWLIST"],
    );

    let mut child = spawn_serve(&["chat", "telegram", "--allow-all", "--chat-config", cfg_str]);

    let done = wait_until(|| {
        telegram.log().iter().any(|r| {
            r.path.contains("/sendMessage") && body_contains(&r.body, "Done.")
        }) && llm.saw_tool_result()
            && telegram.log().iter().any(|r| {
                r.path.contains("/answerCallbackQuery")
                    && body_contains(&r.body, "Only the user who started")
            })
    })
    .await;

    let _ = child.kill().await;
    let output = child.wait_with_output().await.expect("wait for amparo chat");
    drop(env);
    std::fs::remove_file(&cfg).ok();
    std::fs::remove_dir_all(&ws).ok();

    assert!(
        done,
        "the wrong-user press was toasted, then the requester's own press completed the task; \
         child stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        llm.saw_tool_result(),
        "the approved tool ran for real after the requester's press"
    );
}

#[tokio::test]
async fn chat_config_unknown_user_refused() {
    let _guard = LOCK.lock().await;
    let cfg = write_chat_config("unknown-user", "[users.\"telegram:111\"]\n");
    let ws = fresh_workspace("unknown-user");
    let llm = MockLlm::start(vec![]).await;
    let telegram = MockTelegram::start().await;
    telegram.push_update(message_update(501, 222, 222, "do it"));

    let base = telegram.url();
    let llm_url = llm.url();
    let ws_str = ws.to_str().unwrap();
    let cfg_str = cfg.to_str().unwrap();
    let env = set_env(
        &[
            ("AMPARO_CHAT_TELEGRAM_TOKEN", "test-token"),
            ("AMPARO_CHAT_TELEGRAM_BASE", base.as_str()),
            ("AMPARO_INFERENCE_URL", llm_url.as_str()),
            ("AMPARO_INFERENCE_MODEL", "mock-model"),
            ("AMPARO_WORKSPACE", ws_str),
        ],
        &["AMPARO_CHAT_CONFIG", "AMPARO_CHAT_ALLOWLIST"],
    );

    let mut child = spawn_serve(&["chat", "telegram", "--chat-config", cfg_str]);

    let refused = wait_until(|| {
        telegram.log().iter().any(|r| {
            r.path.contains("/sendMessage") && body_contains(&r.body, "not authorized")
        })
    })
    .await;
    let _ = child.kill().await;
    let output = child.wait_with_output().await.expect("wait for amparo chat");
    drop(env);
    std::fs::remove_file(&cfg).ok();
    std::fs::remove_dir_all(&ws).ok();

    assert!(
        refused,
        "222 must be refused; child stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        llm.bodies.lock().unwrap().is_empty(),
        "a refused user never reaches the LLM"
    );
}

#[tokio::test]
async fn chat_config_per_user_trust_ceiling() {
    let _guard = LOCK.lock().await;
    let cfg = write_chat_config(
        "trust-ceiling",
        "[users.\"telegram:111\"]\ntrust_ceiling = \"observational\"\n",
    );
    let ws = fresh_workspace("trust-ceiling");
    let llm = MockLlm::start(vec![tool_call_script("echo hi"), vec![content_frame("Done.")]]).await;
    let telegram = MockTelegram::start().await;
    telegram.push_update(message_update(601, 111, 111, "do it"));

    let base = telegram.url();
    let llm_url = llm.url();
    let ws_str = ws.to_str().unwrap();
    let cfg_str = cfg.to_str().unwrap();
    let env = set_env(
        &[
            ("AMPARO_CHAT_TELEGRAM_TOKEN", "test-token"),
            ("AMPARO_CHAT_TELEGRAM_BASE", base.as_str()),
            ("AMPARO_INFERENCE_URL", llm_url.as_str()),
            ("AMPARO_INFERENCE_MODEL", "mock-model"),
            ("AMPARO_WORKSPACE", ws_str),
        ],
        &["AMPARO_CHAT_CONFIG", "AMPARO_CHAT_ALLOWLIST"],
    );

    let mut child = spawn_serve(&["chat", "telegram", "--allow-all", "--chat-config", cfg_str]);

    // The ceiling blocks run_command before the policy gate, so no approval
    // is ever asked and the task still completes with the scripted answer.
    let done = wait_until(|| {
        telegram.log().iter().any(|r| {
            r.path.contains("/sendMessage") && body_contains(&r.body, "Done.")
        })
    })
    .await;

    let _ = child.kill().await;
    let output = child.wait_with_output().await.expect("wait for amparo chat");
    drop(env);
    std::fs::remove_file(&cfg).ok();
    std::fs::remove_dir_all(&ws).ok();

    assert!(
        done,
        "the task completed despite the blocked call; child stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let log = telegram.log();
    assert!(
        !log.iter().any(|r| r.path.contains("/sendMessage")
            && body_contains(&r.body, "inline_keyboard")),
        "no approval keyboard for a trust-ceiling-blocked call: {log:#?}"
    );
    let bodies = llm.bodies.lock().unwrap();
    assert!(
        bodies.iter().any(|b| b.contains("tool blocked by trust ceiling")),
        "the blocked call was answered to the LLM: {bodies:?}"
    );
    assert!(
        !bodies.iter().any(|b| b.contains("\\\"exit_code\\\":0")),
        "the tool never executed (no success result reached the LLM): {bodies:?}"
    );
}
