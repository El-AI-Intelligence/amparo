//! Integration tests for the Telegram adapter against a scripted mock Bot
//! API server.
//!
//! The mock is a real TCP listener on 127.0.0.1:0 speaking just enough
//! HTTP/1.1 for reqwest: it reads each request head + body, records it into
//! a shared log, answers from a scripted map by method, and REPEATS per
//! connection until the client closes it — reqwest pools connections, so a
//! one-shot server would stall the second request on the same pool.
//!
//! All assertions poll the recorded request log; request order is a
//! transport detail, not a contract.

#[allow(dead_code)]
mod common;
use common::{registry_with_echo, turn_text, turn_tool_call, wait_until, StubProvider};

use amparo_chat::driver::ChatDriver;
use amparo_chat::router::ApprovalRouter;
use amparo_chat::telegram::TelegramTransport;
use amparo_chat::transport::{
    ApprovalButtonPress, ChatError, ChatRef, ChatTransport, PressOutcome,
};
use amparo_policy::AllowAllPolicyEngine;
use amparo_tools::ToolTrustTier;
use serde_json::{json, Value};
use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// One HTTP request the mock answered.
#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    path: String,
    body: String,
}

/// A scripted mock of the Telegram Bot API on 127.0.0.1:0.
struct MockTelegram {
    addr: SocketAddr,
    log: Arc<Mutex<Vec<Recorded>>>,
    updates: Arc<Mutex<VecDeque<Value>>>,
    unauthorized: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl MockTelegram {
    /// Bind a listener and start answering.
    async fn start() -> Arc<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let updates = Arc::new(Mutex::new(VecDeque::new()));
        let unauthorized = Arc::new(AtomicBool::new(false));
        let next_message_id = Arc::new(AtomicI64::new(1000));
        let task = tokio::spawn(serve_mock(
            listener,
            Arc::clone(&log),
            Arc::clone(&updates),
            Arc::clone(&unauthorized),
            Arc::clone(&next_message_id),
        ));
        Arc::new(Self { addr, log, updates, unauthorized, task })
    }

    /// The base URL transports point at.
    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Queue one update as the next `getUpdates` batch (all later batches
    /// are empty).
    fn push_update(&self, update: Value) {
        self.updates.lock().unwrap().push_back(update);
    }

    /// Make every `getUpdates` answer `ok: false` with `error_code` 401.
    fn unauthorized(&self) {
        self.unauthorized.store(true, Ordering::SeqCst);
    }

    /// Every recorded request, in arrival order.
    fn log(&self) -> Vec<Recorded> {
        self.log.lock().unwrap().clone()
    }

    /// Stop the accept loop.
    fn stop(&self) {
        self.task.abort();
    }
}

/// Accept connections and answer each on its own task.
async fn serve_mock(
    listener: TcpListener,
    log: Arc<Mutex<Vec<Recorded>>>,
    updates: Arc<Mutex<VecDeque<Value>>>,
    unauthorized: Arc<AtomicBool>,
    next_message_id: Arc<AtomicI64>,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else { return };
        tokio::spawn(serve_connection(
            stream,
            Arc::clone(&log),
            Arc::clone(&updates),
            Arc::clone(&unauthorized),
            Arc::clone(&next_message_id),
        ));
    }
}

/// Answer requests on one connection until the client closes it (reqwest
/// keep-alive: one connection carries many requests).
async fn serve_connection(
    stream: tokio::net::TcpStream,
    log: Arc<Mutex<Vec<Recorded>>>,
    updates: Arc<Mutex<VecDeque<Value>>>,
    unauthorized: Arc<AtomicBool>,
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
            "getUpdates" if unauthorized.load(Ordering::SeqCst) => {
                json!({"ok": false, "error_code": 401, "description": "Unauthorized"})
            }
            "getUpdates" => match updates.lock().unwrap().pop_front() {
                Some(update) => json!({"ok": true, "result": [update]}),
                None => json!({"ok": true, "result": []}),
            },
            "sendMessage" => json!({
                "ok": true,
                "result": {"message_id": next_message_id.fetch_add(1, Ordering::SeqCst)}
            }),
            "editMessageText" => json!({"ok": true, "result": true}),
            "answerCallbackQuery" => json!({"ok": true, "result": true}),
            other => {
                json!({"ok": false, "error_code": 404, "description": format!("unknown method {other}")})
            }
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

/// A driver wired like `amparo_chat::dispatch::build_driver` — allowlist,
/// provider, policy, registry, driver — but with test doubles instead of
/// the environment surface.
fn test_driver(
    allowlist: HashSet<String>,
    provider: Arc<StubProvider>,
    transport: Arc<dyn ChatTransport>,
    auto_approve: bool,
) -> Arc<ChatDriver> {
    Arc::new(
        ChatDriver::new(
            allowlist,
            provider,
            Arc::new(AllowAllPolicyEngine),
            registry_with_echo(ToolTrustTier::ExternalEffector),
            PathBuf::from("."),
            transport,
            Arc::new(ApprovalRouter::new()),
            auto_approve,
        )
        .with_trust_ceiling(ToolTrustTier::SystemControl),
    )
}

/// The full path: a message starts a task, the task escalates the echo call
/// to the gate, the gate sends an inline-keyboard approval, the press is
/// routed back, the message is edited with the outcome, and the final
/// answer is delivered.
#[tokio::test]
async fn message_to_approval_to_answer_roundtrip() {
    let mock = MockTelegram::start().await;
    let transport: Arc<dyn ChatTransport> =
        Arc::new(TelegramTransport::new(mock.url(), "TEST-TOKEN-1"));
    let driver = test_driver(
        HashSet::from(["111".to_string()]),
        StubProvider::new(vec![
            turn_tool_call("call_1", "echo", r#"{"message":"hi"}"#),
            turn_text("Done."),
        ]),
        transport,
        false, // no auto-approve — the gate must fire
    );

    driver
        .on_message(
            ChatRef { platform: "telegram", chat_id: "222".into(), user_id: "111".into() },
            "do a thing".into(),
        )
        .await;

    // The echo call escalates (external-effector tier), so the gate sends a
    // sendMessage whose inline keyboard carries our button payloads.
    wait_until(|| {
        mock.log().iter().any(|r| {
            r.method == "POST"
                && r.path.contains("/sendMessage")
                && body_contains(&r.body, "approve:call_1")
                && body_contains(&r.body, "deny:call_1")
        })
    })
    .await;

    // Simulate the human pressing Approve, the way the receive loop would
    // route the callback query.
    let press = ApprovalButtonPress {
        chat_id: "222".into(),
        approval_id: "call_1".into(),
        approved: true,
        user_id: "111".into(),
    };
    assert_eq!(
        driver.on_approval(press).await,
        PressOutcome::Routed,
        "the press reached the waiting gate"
    );

    // The gate edits the approval message with the outcome (removing the
    // buttons) and the final answer arrives as a plain sendMessage.
    wait_until(|| mock.log().iter().any(|r| r.path.contains("/editMessageText"))).await;
    wait_until(|| {
        mock.log()
            .iter()
            .any(|r| r.path.contains("/sendMessage") && body_contains(&r.body, "Done."))
    })
    .await;

    mock.stop();
}

/// The receive loop: long-polls the mock with advancing offsets on the
/// token path, feeds messages and callback queries to the driver, and
/// always answers callback queries so the client spinner stops.
#[tokio::test]
async fn receive_loop_polls_offsets_and_feeds_the_driver() {
    let mock = MockTelegram::start().await;
    mock.push_update(message_update(101, 111, 222, "hello there"));
    mock.push_update(callback_update(102, 111, 222, "approve:call_1"));

    let transport: Arc<dyn ChatTransport> =
        Arc::new(TelegramTransport::new(mock.url(), "TEST-TOKEN-2"));
    let driver = test_driver(
        HashSet::from(["111".to_string()]),
        StubProvider::new(vec![]),
        transport.clone(),
        true,
    );

    let loop_task = tokio::spawn({
        let transport = Arc::clone(&transport);
        async move { transport.receive(driver).await }
    });

    // The token path was hit (paths read /botTEST-TOKEN-2/...) and the
    // first poll carries no offset. Offsets travel in the form BODY, not
    // the query string — the receive loop posts Vec<(String, String)>
    // params form-encoded, so assertions decode the recorded body.
    wait_until(|| {
        mock.log().iter().any(|r| r.path.starts_with("/botTEST-TOKEN-2/getUpdates"))
            && mock.log()
                .iter()
                .any(|r| r.path.contains("/getUpdates") && !body_contains(&r.body, "offset="))
    })
    .await;

    // Offsets advance one past the last confirmed update id: 101 → 102 → 103.
    wait_until(|| mock.log().iter().any(|r| body_contains(&r.body, "offset=102"))).await;
    wait_until(|| mock.log().iter().any(|r| body_contains(&r.body, "offset=103"))).await;

    // The message update started a task (the final answer was delivered)
    // and the orphan callback was answered as already decided.
    wait_until(|| {
        mock.log()
            .iter()
            .any(|r| r.path.contains("/sendMessage") && body_contains(&r.body, "Done."))
    })
    .await;
    wait_until(|| {
        mock.log().iter().any(|r| {
            r.path.contains("/answerCallbackQuery")
                && body_contains(&r.body, "Already decided")
        })
    })
    .await;

    loop_task.abort();
    let _ = loop_task.await;
    mock.stop();
}

/// A callback from a user who did not start the task is answered with the
/// wrong-user toast, the pending approval is NOT consumed, and the
/// requester's own later callback still routes the decision.
#[tokio::test]
async fn callback_from_another_user_gets_the_wrong_user_toast() {
    let mock = MockTelegram::start().await;
    mock.push_update(message_update(201, 111, 222, "do a thing"));

    let transport: Arc<dyn ChatTransport> =
        Arc::new(TelegramTransport::new(mock.url(), "TEST-TOKEN-4"));
    let driver = test_driver(
        HashSet::from(["111".to_string()]),
        StubProvider::new(vec![
            turn_tool_call("call_1", "echo", r#"{"message":"hi"}"#),
            turn_text("Done."),
        ]),
        transport.clone(),
        false, // no auto-approve — the gate must fire
    );

    let loop_task = tokio::spawn({
        let transport = Arc::clone(&transport);
        async move { transport.receive(driver).await }
    });

    // The task's inline keyboard is on screen (and the gate registered).
    wait_until(|| {
        mock.log().iter().any(|r| {
            r.path.contains("/sendMessage") && body_contains(&r.body, "approve:call_1")
        })
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // A different user presses Approve: the toast tells them the decision
    // is not theirs — the pending approval survives.
    mock.push_update(callback_update(202, 222, 222, "approve:call_1"));
    wait_until(|| {
        mock.log().iter().any(|r| {
            r.path.contains("/answerCallbackQuery")
                && body_contains(&r.body, amparo_chat::driver::WRONG_USER_TOAST)
        })
    })
    .await;

    // The requester's own press still routes and the task finishes.
    mock.push_update(callback_update(203, 111, 222, "approve:call_1"));
    wait_until(|| {
        mock.log()
            .iter()
            .any(|r| r.path.contains("/sendMessage") && body_contains(&r.body, "Done."))
    })
    .await;

    loop_task.abort();
    let _ = loop_task.await;
    mock.stop();
}

/// A rejected token (error_code 401) is `ChatError::Fatal` — the loop exits
/// immediately instead of retrying forever.
#[tokio::test]
async fn unauthorized_token_is_fatal_without_retry() {
    let mock = MockTelegram::start().await;
    mock.unauthorized();
    let transport: Arc<dyn ChatTransport> =
        Arc::new(TelegramTransport::new(mock.url(), "TEST-TOKEN-3"));
    let driver = test_driver(HashSet::new(), StubProvider::new(vec![]), transport.clone(), true);

    let result = transport.receive(driver).await;
    assert!(
        matches!(result, Err(ChatError::Fatal(_))),
        "401 must be fatal, got {result:?}"
    );

    // Fail-closed: exactly one getUpdates, no 5-second retry.
    let updates = mock.log().iter().filter(|r| r.path.contains("/getUpdates")).count();
    assert_eq!(updates, 1, "the 401 must not be retried");
    mock.stop();
}
