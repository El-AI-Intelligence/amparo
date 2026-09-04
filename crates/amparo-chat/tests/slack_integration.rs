//! Integration tests for the Slack adapter: a mock Slack Web API (plain
//! HTTP) and a mock Socket Mode websocket, driven end-to-end through a real
//! [`ChatDriver`] with the shared [`StubProvider`] and an echo tool.
//!
//! The two mocks append to one shared ordered log, so the tests can assert
//! the ACK-BEFORE-PROCESSING contract: an envelope is acknowledged on the
//! websocket before the driver does anything visible (postMessage, update,
//! response_url).

#[allow(dead_code)]
mod common;

use amparo_chat::dispatch::{ChatFlags, Platform};
use amparo_chat::driver::{ChatDriver, PolicySource, Tenants};
use amparo_chat::router::ApprovalRouter;
use amparo_chat::slack::SlackTransport;
use amparo_chat::transport::{ChatError, ChatTransport};
use amparo_policy::AllowAllPolicyEngine;
use amparo_tools::ToolTrustTier;
use common::{registry_with_echo, turn_text, turn_tool_call, wait_until, StubProvider};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// Serializes the env-mutating serve test against any sibling test that
/// touches the same variables.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Shared state between the mock HTTP server and the mock websocket: an
/// ordered event log (both append to it) and the recorded request bodies.
#[derive(Default)]
struct MockState {
    log: Mutex<Vec<String>>,
    calls: Mutex<Vec<(String, String)>>,
}

/// A mock Slack Web API on a local port: `apps.connections.open` hands out
/// `ws_url`, `chat.postMessage`/`chat.update` record their bodies and
/// return minimal ok with a ts counter, and any `/responses/…` path stands
/// in for a `response_url` cleanup POST.
async fn start_http_mock(state: Arc<MockState>, ws_url: String) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind http mock");
    let port = listener.local_addr().expect("mock addr").port();
    let ts = Arc::new(AtomicUsize::new(1));
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else { break };
            let state = Arc::clone(&state);
            let ws_url = ws_url.clone();
            let ts = Arc::clone(&ts);
            tokio::spawn(async move {
                let mut sock = sock;
                // Loop per connection until the client closes it — reqwest
                // pools connections, and every request must be recorded.
                loop {
                    let Some((path, body)) = read_request(&mut sock).await else { break };
                    state.calls.lock().unwrap().push((path.clone(), body));
                    let response = match path.as_str() {
                        "/api/apps.connections.open" => {
                            state.log.lock().unwrap().push("connections.open".into());
                            json!({"ok": true, "url": ws_url}).to_string()
                        }
                        "/api/chat.postMessage" => {
                            state.log.lock().unwrap().push("chat.postMessage".into());
                            let n = ts.fetch_add(1, Ordering::SeqCst);
                            json!({"ok": true, "ts": format!("{n}.0"), "channel": "C1"}).to_string()
                        }
                        "/api/chat.update" => {
                            state.log.lock().unwrap().push("chat.update".into());
                            json!({"ok": true, "ts": "1.0", "channel": "C1"}).to_string()
                        }
                        p if p.starts_with("/responses/") => {
                            state.log.lock().unwrap().push("response_url".into());
                            json!({"ok": true}).to_string()
                        }
                        p => {
                            state.log.lock().unwrap().push(format!("unexpected:{p}"));
                            json!({"ok": false, "error": "not found"}).to_string()
                        }
                    };
                    write_response(&mut sock, &response).await;
                }
            });
        }
    });
    port
}

/// Read one HTTP request (head by `\r\n\r\n`, body by Content-Length), or
/// `None` when the connection is closed.
async fn read_request(sock: &mut tokio::net::TcpStream) -> Option<(String, String)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        match sock.read(&mut tmp).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
        if buf.len() > 1_000_000 {
            return None;
        }
    }
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .unwrap_or(buf.len());
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
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
    Some((path, String::from_utf8_lossy(&body).to_string()))
}

/// Write a minimal 200 JSON response.
async fn write_response(sock: &mut tokio::net::TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = sock.write_all(response.as_bytes()).await;
}

/// A Socket Mode envelope carrying `payload` (none for hello/disconnect).
fn envelope(id: &str, kind: &str, payload: Option<Value>) -> String {
    let mut e = json!({"envelope_id": id, "type": kind});
    if let Some(p) = payload {
        e["payload"] = p;
    }
    e.to_string()
}

/// An `events_api` envelope carrying a real user message.
fn user_message(id: &str, channel: &str, user: &str, text: &str) -> String {
    envelope(
        id,
        "events_api",
        Some(json!({"type": "message", "channel": channel, "user": user, "text": text, "ts": "1.0"})),
    )
}

/// An `events_api` envelope carrying a bot-originated message (the echo).
fn bot_message(id: &str, channel: &str, text: &str) -> String {
    envelope(
        id,
        "events_api",
        Some(json!({"type": "message", "channel": channel, "text": text, "bot_id": "B1"})),
    )
}

/// An `interactive` envelope carrying a `block_actions` button press from
/// `user`.
fn button_press(id: &str, user: &str, action_id: &str, channel: &str, response_url: &str) -> String {
    envelope(
        id,
        "interactive",
        Some(json!({
            "type": "block_actions",
            "user": {"id": user, "username": "tester"},
            "channel": {"id": channel},
            "actions": [{"action_id": action_id, "block_id": "b1"}],
            "response_url": response_url,
        })),
    )
}

/// Wait until a call to `path` whose body contains `needle` was recorded.
async fn wait_for_body(state: &Arc<MockState>, path: &str, needle: &str) {
    let state = Arc::clone(state);
    wait_until(move || {
        state
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|(p, b)| p == path && b.contains(needle))
    })
    .await;
}

/// Wait until the ordered log records `entry`.
async fn wait_for_log(state: &Arc<MockState>, entry: &str) {
    let state = Arc::clone(state);
    wait_until(move || state.log.lock().unwrap().iter().any(|e| e == entry)).await;
}

/// Wait until no new log entry appears for `quiet_for` — the signal that a
/// burst of outbound activity is fully drained.
async fn wait_quiet(state: &Arc<MockState>, quiet_for: Duration) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let before = state.log.lock().unwrap().clone();
        tokio::time::sleep(quiet_for).await;
        let after = state.log.lock().unwrap().clone();
        if after == before {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "outbound never went quiet: {after:?}"
        );
    }
}

/// The first recorded call to `path` whose body contains `needle`.
fn body_of(state: &MockState, path: &str, needle: &str) -> String {
    state
        .calls
        .lock()
        .unwrap()
        .iter()
        .find(|(p, b)| p == path && b.contains(needle))
        .map(|(_, b)| b.clone())
        .unwrap_or_else(|| panic!("no recorded call to {path} containing {needle:?}"))
}

/// A real driver: allowlist {"U333"} — the user whose presses may decide
/// approvals, matching the button-press envelopes — an echo tool at
/// ExternalEffector tier (so the approval gate fires), approvals NOT
/// auto-granted.
fn driver(transport: Arc<dyn ChatTransport>) -> Arc<ChatDriver> {
    Arc::new(ChatDriver::new(
        Tenants::LegacyAllowlist(HashSet::from(["U333".to_string()])),
        StubProvider::new(vec![
            turn_tool_call("call_1", "echo", r#"{"message":"hi"}"#),
            turn_text("Done."),
        ]),
        PolicySource::Shared(Arc::new(AllowAllPolicyEngine)),
        registry_with_echo(ToolTrustTier::ExternalEffector),
        PathBuf::from("/tmp/amparo-chat-test"),
        transport,
        Arc::new(ApprovalRouter::new()),
        false,
    ))
}

/// A transport against the local mocks with fake tokens; `base` is the
/// mock's `/api` root.
fn test_transport(base: String) -> Arc<dyn ChatTransport> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("http client");
    Arc::new(SlackTransport::new(base, "xapp-test-token".into(), "xoxb-test-token".into(), http))
}

/// Read the next text frame on the mock websocket and log it as
/// `ack:<envelope_id>` — the mock Slack server's side of the ack contract.
async fn read_ack(ws: &mut WebSocketStream<tokio::net::TcpStream>, log: &Mutex<Vec<String>>) {
    loop {
        let frame = ws.next().await.expect("websocket stays open");
        if let Message::Text(text) = frame.expect("clean frame") {
            let id = serde_json::from_str::<Value>(text.as_str())
                .ok()
                .and_then(|v| v["envelope_id"].as_str().map(str::to_string))
                .unwrap_or_else(|| "<no-id>".to_string());
            log.lock().unwrap().push(format!("ack:{id}"));
            return;
        }
    }
}

#[tokio::test]
async fn socket_mode_acks_before_processing_and_round_trips_approval() {
    let state = Arc::new(MockState::default());
    let ws_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ws mock");
    let ws_port = ws_listener.local_addr().expect("ws addr").port();
    let http_port =
        start_http_mock(Arc::clone(&state), format!("ws://127.0.0.1:{ws_port}/link")).await;
    let response_url = format!("http://127.0.0.1:{http_port}/responses/hooks/xyz");
    let transport = test_transport(format!("http://127.0.0.1:{http_port}/api"));
    let driver = driver(Arc::clone(&transport));

    // The mock websocket drives the script: hello, a real message, an
    // Approve press, a bot-originated echo — reading each ack as it lands.
    let ws_state = Arc::clone(&state);
    let ws_handle = tokio::spawn(async move {
        let (stream, _) = ws_listener.accept().await.expect("ws client connected");
        let mut ws = tokio_tungstenite::accept_async(stream).await.expect("ws handshake");
        ws.send(Message::text(r#"{"type":"hello"}"#.to_string()))
            .await
            .expect("send hello");

        // 1. A real user message: the ack must be logged before ANY outbound.
        ws.send(Message::text(user_message("ev_1", "C1", "U333", "do a thing")))
            .await
            .expect("send ev_1");
        read_ack(&mut ws, &ws_state.log).await; // "ack:ev_1"

        // Wait until the driver posted the approval (buttons on screen) and
        // the gate registered it, then press Approve.
        wait_for_body(&ws_state, "/api/chat.postMessage", "approve:call_1").await;
        wait_quiet(&ws_state, Duration::from_millis(200)).await;
        ws.send(Message::text(button_press("act_1", "U333", "approve:call_1", "C1", &response_url)))
            .await
            .expect("send act_1");
        read_ack(&mut ws, &ws_state.log).await; // "ack:act_1"

        // Wait for the gate's edit, then for the final answer, and let the
        // event-sink drain flush — so nothing is in flight when the echo lands.
        wait_for_body(&ws_state, "/api/chat.update", "\"Approved\"").await;
        wait_for_body(&ws_state, "/api/chat.postMessage", "\"text\":\"Done.\"").await;
        wait_quiet(&ws_state, Duration::from_millis(300)).await;

        // 3. A bot-originated message: acked, and nothing may follow it.
        ws.send(Message::text(bot_message("ev_2", "C1", "ignore me")))
            .await
            .expect("send ev_2");
        read_ack(&mut ws, &ws_state.log).await; // "ack:ev_2"
    });

    // The receive loop is the unit under test; it runs until the runtime
    // drops it at the end of the test.
    let recv = Arc::clone(&transport);
    tokio::spawn(async move { recv.receive(Arc::clone(&driver)).await });

    tokio::time::timeout(Duration::from_secs(15), ws_handle)
        .await
        .expect("ws mock script timed out")
        .expect("ws mock script failed");

    // Nothing else may arrive after the bot-message ack.
    wait_quiet(&state, Duration::from_millis(300)).await;
    let log = state.log.lock().unwrap().clone();
    assert_eq!(log.first().map(String::as_str), Some("connections.open"), "{log:?}");
    let pos = |entry: &str| {
        log.iter()
            .position(|e| e == entry)
            .unwrap_or_else(|| panic!("{entry} missing from log: {log:?}"))
    };
    let first_outbound = log
        .iter()
        .position(|e| e.starts_with("chat."))
        .expect("some outbound call");
    assert!(pos("ack:ev_1") < first_outbound, "ack before any outbound: {log:?}");
    assert!(pos("ack:act_1") < pos("chat.update"), "ack before the gate edit: {log:?}");
    assert_eq!(
        log.last().map(String::as_str),
        Some("ack:ev_2"),
        "nothing after the echo ack: {log:?}"
    );

    // The recorded bodies carry the round-trip: buttons on the approval,
    // the gate's edit removing them, the final answer delivered.
    let approval = body_of(&state, "/api/chat.postMessage", "approve:call_1");
    assert!(
        approval.contains("deny:call_1")
            && approval.contains("\"Approve\"")
            && approval.contains("\"Deny\""),
        "approval blocks: {approval}"
    );
    let update = body_of(&state, "/api/chat.update", "\"Approved\"");
    assert!(
        update.contains("\"blocks\":[]")
            && update.contains("\"channel\":\"C1\"")
            && update.contains("\"ts\":"),
        "gate edit: {update}"
    );
    let answer = body_of(&state, "/api/chat.postMessage", "\"text\":\"Done.\"");
    assert!(answer.contains("Done."), "final answer: {answer}");
}

#[tokio::test]
async fn already_decided_press_cleans_up_via_response_url() {
    let state = Arc::new(MockState::default());
    let ws_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ws mock");
    let ws_port = ws_listener.local_addr().expect("ws addr").port();
    let http_port =
        start_http_mock(Arc::clone(&state), format!("ws://127.0.0.1:{ws_port}/link")).await;
    let response_url = format!("http://127.0.0.1:{http_port}/responses/hooks/xyz");
    let transport = test_transport(format!("http://127.0.0.1:{http_port}/api"));
    let driver = driver(Arc::clone(&transport));

    // A press for an approval that never existed: no gate is waiting, so
    // the adapter must ack and clean the message up via `response_url`.
    let ws_state = Arc::clone(&state);
    let ws_handle = tokio::spawn(async move {
        let (stream, _) = ws_listener.accept().await.expect("ws client connected");
        let mut ws = tokio_tungstenite::accept_async(stream).await.expect("ws handshake");
        ws.send(Message::text(r#"{"type":"hello"}"#.to_string()))
            .await
            .expect("send hello");
        ws.send(Message::text(button_press("act_ghost", "U333", "approve:ghost", "C1", &response_url)))
            .await
            .expect("send press");
        read_ack(&mut ws, &ws_state.log).await; // "ack:act_ghost"
    });

    let recv = Arc::clone(&transport);
    tokio::spawn(async move { recv.receive(Arc::clone(&driver)).await });

    tokio::time::timeout(Duration::from_secs(15), ws_handle)
        .await
        .expect("ws mock script timed out")
        .expect("ws mock script failed");
    wait_for_log(&state, "response_url").await;
    wait_quiet(&state, Duration::from_millis(200)).await;

    let log = state.log.lock().unwrap().clone();
    let pos = |entry: &str| {
        log.iter()
            .position(|e| e == entry)
            .unwrap_or_else(|| panic!("{entry} missing from log: {log:?}"))
    };
    assert!(pos("ack:act_ghost") < pos("response_url"), "ack before cleanup: {log:?}");
    assert!(
        !log.iter().any(|e| e.starts_with("chat.")),
        "an already-decided press must not send or edit messages: {log:?}"
    );
    let cleanup = body_of(&state, "/responses/hooks/xyz", "Already decided");
    assert!(cleanup.contains("replace_original"), "cleanup body: {cleanup}");
}

/// Serve-flags for the fail-closed test — the Slack platform with
/// everything permissive, so a missing token is the only thing to trip on.
fn flags() -> ChatFlags {
    ChatFlags {
        platform: Platform::Slack,
        policy_url: None,
        allow_all: true,
        auto_approve: true,
        trust_ceiling: ToolTrustTier::SystemControl,
        chat_config: None,
        growth: false,
        receiver: None,
    }
}

#[tokio::test]
async fn serve_fails_closed_without_tokens() {
    let _guard = ENV_LOCK.lock().unwrap();
    let app = std::env::var("AMPARO_CHAT_SLACK_APP_TOKEN").ok();
    let bot = std::env::var("AMPARO_CHAT_SLACK_BOT_TOKEN").ok();
    std::env::remove_var("AMPARO_CHAT_SLACK_APP_TOKEN");
    std::env::remove_var("AMPARO_CHAT_SLACK_BOT_TOKEN");

    let err = amparo_chat::slack::serve(&flags())
        .await
        .expect_err("missing tokens must fail closed");
    match &err {
        ChatError::Fatal(message) => {
            assert!(message.contains("AMPARO_CHAT_SLACK_APP_TOKEN"), "{message}");
            assert!(message.contains("AMPARO_CHAT_SLACK_BOT_TOKEN"), "{message}");
        }
        other => panic!("expected Fatal for missing tokens, got {other:?}"),
    }

    for (key, value) in
        [("AMPARO_CHAT_SLACK_APP_TOKEN", app), ("AMPARO_CHAT_SLACK_BOT_TOKEN", bot)]
    {
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
}

/// A press from a user who did not start the task is answered with an
/// ephemeral toast on the response_url — never a replace_original, which
/// would strip the requester's buttons — and the requester's own press
/// still routes.
#[tokio::test]
async fn wrong_user_press_gets_ephemeral_toast_and_requester_still_decides() {
    let state = Arc::new(MockState::default());
    let ws_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ws mock");
    let ws_port = ws_listener.local_addr().expect("ws addr").port();
    let http_port =
        start_http_mock(Arc::clone(&state), format!("ws://127.0.0.1:{ws_port}/link")).await;
    let response_url = format!("http://127.0.0.1:{http_port}/responses/hooks/xyz");
    let transport = test_transport(format!("http://127.0.0.1:{http_port}/api"));
    let driver = driver(Arc::clone(&transport));

    let ws_state = Arc::clone(&state);
    let ws_handle = tokio::spawn(async move {
        let (stream, _) = ws_listener.accept().await.expect("ws client connected");
        let mut ws = tokio_tungstenite::accept_async(stream).await.expect("ws handshake");
        ws.send(Message::text(r#"{"type":"hello"}"#.to_string()))
            .await
            .expect("send hello");

        ws.send(Message::text(user_message("ev_1", "C1", "U333", "do a thing")))
            .await
            .expect("send ev_1");
        read_ack(&mut ws, &ws_state.log).await; // "ack:ev_1"

        wait_for_body(&ws_state, "/api/chat.postMessage", "approve:call_1").await;
        wait_quiet(&ws_state, Duration::from_millis(200)).await;

        // A different user presses Approve — refused with an ephemeral
        // toast; the pending approval is not consumed.
        ws.send(Message::text(button_press("act_1", "U999", "approve:call_1", "C1", &response_url)))
            .await
            .expect("send act_1");
        read_ack(&mut ws, &ws_state.log).await; // "ack:act_1"
        wait_for_body(&ws_state, "/responses/hooks/xyz", "ephemeral").await;

        // The requester's own press still routes the decision.
        ws.send(Message::text(button_press("act_2", "U333", "approve:call_1", "C1", &response_url)))
            .await
            .expect("send act_2");
        read_ack(&mut ws, &ws_state.log).await; // "ack:act_2"
        wait_for_body(&ws_state, "/api/chat.update", "\"Approved\"").await;
        wait_for_body(&ws_state, "/api/chat.postMessage", "\"text\":\"Done.\"").await;
    });

    let recv = Arc::clone(&transport);
    tokio::spawn(async move { recv.receive(Arc::clone(&driver)).await });

    tokio::time::timeout(Duration::from_secs(15), ws_handle)
        .await
        .expect("ws mock script timed out")
        .expect("ws mock script failed");
    wait_quiet(&state, Duration::from_millis(300)).await;

    let toast = body_of(&state, "/responses/hooks/xyz", "response_type");
    assert!(
        toast.contains("\"response_type\":\"ephemeral\"")
            && toast.contains("Only the user who started the task can decide."),
        "ephemeral toast body: {toast}"
    );
    assert!(
        !toast.contains("replace_original"),
        "never replace the requester's buttons: {toast}"
    );

    let update = body_of(&state, "/api/chat.update", "\"Approved\"");
    assert!(update.contains("\"blocks\":[]"), "the requester's press routed: {update}");
    let answer = body_of(&state, "/api/chat.postMessage", "\"text\":\"Done.\"");
    assert!(answer.contains("Done."), "final answer: {answer}");
}
