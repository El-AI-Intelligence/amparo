//! Discord adapter integration tests against mock gateway and REST servers.
//!
//! The gateway mock is a real websocket server (TcpListener +
//! `accept_async`) running a small script: send frames, expect opcodes
//! (auto-acking stray heartbeats), accept a reconnection, and wait on the
//! shared REST log — every step bounded by a timeout so a broken adapter
//! fails the test instead of hanging it. The REST mock is a plain HTTP
//! server recording every request (method, path, body, raw head for
//! Authorization assertions) that can script one 429 on the first message
//! post. Nothing here touches the real Discord API.

// The shared doubles carry helpers other adapters' tests use (MockTransport,
// wait_for_text, panicking) — silenced here per the file's own header, which
// tells integration consumers to mark the module.
#[allow(dead_code)]
mod common;

use amparo_agent::{ApprovalRequest, BlastRadius};
use amparo_chat::discord::DiscordTransport;
use amparo_chat::driver::{ChatDriver, PolicySource, Tenants};
use amparo_chat::router::ApprovalRouter;
use amparo_chat::transport::{ChatRef, ChatTransport};
use amparo_policy::AllowAllPolicyEngine;
use amparo_tools::ToolTrustTier;
use common::{registry_with_echo, turn_text, turn_tool_call, wait_until, StubProvider};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{accept_async, WebSocketStream};

/// The overall budget for every gateway script — the test must finish well
/// inside this or fail.
const SCRIPT_BUDGET: Duration = Duration::from_secs(30);
/// How long one expected frame may take to arrive.
const FRAME_BUDGET: Duration = Duration::from_secs(10);

/// The mock gateway's HELLO: a 200ms heartbeat interval.
fn hello_frame() -> Value {
    json!({ "op": 10, "d": { "heartbeat_interval": 200 } })
}

/// A heartbeat acknowledgement frame.
fn ack_frame() -> Value {
    json!({ "op": 11, "d": null })
}

/// A gateway script step.
enum GwStep {
    /// Send this raw gateway frame.
    Send(Value),
    /// Read frames until `op` arrives (stray heartbeats are auto-acked),
    /// then run `check` on the frame — a failed check fails the script.
    Expect { op: u8, check: Arc<dyn Fn(&Value) -> Result<(), String> + Send + Sync> },
    /// Close the current connection, accept the next one, and send HELLO;
    /// the following steps run on the new connection.
    Reconnect,
    /// Poll the shared REST log until `cond` holds (or the script budget
    /// runs out) — used to time an event to an outbound call.
    WaitRest(Arc<dyn Fn(&[RecordedRequest]) -> bool + Send + Sync>),
}

/// An `Expect` step with a plain check closure.
fn expect(
    op: u8,
    check: impl Fn(&Value) -> Result<(), String> + Send + Sync + 'static,
) -> GwStep {
    GwStep::Expect { op, check: Arc::new(check) }
}

/// A `WaitRest` step with a plain predicate.
fn wait_rest(cond: impl Fn(&[RecordedRequest]) -> bool + Send + Sync + 'static) -> GwStep {
    GwStep::WaitRest(Arc::new(cond))
}

/// A tiny assertion combinator for script steps: `false` fails the script
/// with `message` instead of panicking inside the spawned mock task.
fn check(ok: bool, message: &str) -> Result<(), String> {
    if ok {
        Ok(())
    } else {
        Err(message.to_string())
    }
}

/// One HTTP request the REST mock recorded.
#[derive(Clone, Debug)]
struct RecordedRequest {
    method: String,
    path: String,
    body: String,
    /// The raw request head — for Authorization-header assertions.
    head: String,
}

/// A mock Discord REST server: records every request, answers message
/// posts with `{"id": "mock_msg_N"}` and everything else with `{}`. With
/// `rate_limited_first` the first POST to a message endpoint answers 429
/// with a small `retry_after` so the adapter's retry is observable without
/// a 50-second sleep.
struct MockRest {
    log: Arc<Mutex<Vec<RecordedRequest>>>,
    message_posts: AtomicU64,
    rate_limited_first: bool,
    addr: std::net::SocketAddr,
}

impl MockRest {
    async fn start(rate_limited_first: bool) -> Arc<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock rest");
        let addr = listener.local_addr().expect("mock rest addr");
        let this = Arc::new(Self {
            log: Arc::new(Mutex::new(Vec::new())),
            message_posts: AtomicU64::new(0),
            rate_limited_first,
            addr,
        });
        let me = Arc::clone(&this);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let me = Arc::clone(&me);
                tokio::spawn(async move {
                    let Some(request) = read_request(&mut sock).await else { return };
                    let response = me.response_for(&request);
                    me.log.lock().expect("rest log").push(request);
                    let _ = sock.write_all(response.as_bytes()).await;
                });
            }
        });
        this
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The scripted response for a request: the first message post answers
    /// 429 when configured, later ones 200 with a message id.
    fn response_for(&self, request: &RecordedRequest) -> String {
        let is_message_post = request.method == "POST"
            && request.path.starts_with("/channels/")
            && request.path.ends_with("/messages");
        if is_message_post {
            // One increment per message post; the pre-increment value is
            // the post's number — the first one gets the 429, the retry
            // becomes mock_msg_1.
            let n = self.message_posts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.rate_limited_first && n == 0 {
                let body = json!({ "message": "rate limited", "retry_after": 0.05, "global": false })
                    .to_string();
                return format!(
                    "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
            let body = json!({ "id": format!("mock_msg_{n}") }).to_string();
            return format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
        }
        let body = json!({}).to_string();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// Every request recorded so far, in arrival order.
    fn requests(&self) -> Vec<RecordedRequest> {
        self.log.lock().expect("rest log").clone()
    }
}

/// The mock gateway: runs a [`GwStep`] script against a real websocket
/// server and reports the outcome (assertion failures included) through a
/// oneshot the test awaits with a timeout.
struct MockGateway {
    result: tokio::sync::oneshot::Receiver<Result<(), String>>,
    addr: std::net::SocketAddr,
}

impl MockGateway {
    async fn start(steps: Vec<GwStep>, rest_log: Arc<Mutex<Vec<RecordedRequest>>>) -> MockGateway {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock gateway");
        let addr = listener.local_addr().expect("mock gateway addr");
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let outcome = run_gateway_script(listener, rest_log, steps).await;
            let _ = tx.send(outcome);
        });
        MockGateway { result: rx, addr }
    }

    fn ws_url(&self) -> String {
        format!("ws://{}", self.addr)
    }

    /// The script's outcome: `Ok` on success, the failure message
    /// otherwise. Bounded by [`SCRIPT_BUDGET`] so a hung script cannot hang
    /// the test.
    async fn finished(self) -> Result<(), String> {
        match tokio::time::timeout(SCRIPT_BUDGET, self.result).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => Err("gateway script task vanished".into()),
            Err(_) => Err(format!("gateway script did not finish within {SCRIPT_BUDGET:?}")),
        }
    }
}

/// Run a gateway script: accept a connection, send HELLO, then execute the
/// steps. Every step is bounded; the script ends with a closed connection.
async fn run_gateway_script(
    listener: TcpListener,
    rest_log: Arc<Mutex<Vec<RecordedRequest>>>,
    steps: Vec<GwStep>,
) -> Result<(), String> {
    let (sock, _) = listener.accept().await.map_err(|e| format!("accept: {e}"))?;
    let mut ws = accept_async(sock).await.map_err(|e| format!("ws handshake: {e}"))?;
    send_frame(&mut ws, &hello_frame()).await?;

    for step in steps {
        match step {
            GwStep::Send(frame) => send_frame(&mut ws, &frame).await?,
            GwStep::Reconnect => {
                let (sock, _) = listener.accept().await.map_err(|e| format!("re-accept: {e}"))?;
                ws = accept_async(sock).await.map_err(|e| format!("re-handshake: {e}"))?;
                send_frame(&mut ws, &hello_frame()).await?;
            }
            GwStep::Expect { op, check } => {
                let frame = wait_for_frame(&mut ws, op).await?;
                // Checks inspect the payload (`d`), not the frame envelope.
                check(&frame["d"])?;
            }
            GwStep::WaitRest(cond) => {
                let deadline = tokio::time::Instant::now() + SCRIPT_BUDGET;
                loop {
                    let reqs = rest_log.lock().expect("rest log").clone();
                    if cond(&reqs) {
                        break;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Err("WaitRest condition never held".to_string());
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
    }
    Ok(())
}

/// Send one frame, bounded so a stuck socket cannot hang the script.
async fn send_frame(ws: &mut WebSocketStream<TcpStream>, frame: &Value) -> Result<(), String> {
    tokio::time::timeout(FRAME_BUDGET, ws.send(WsMessage::Text(frame.to_string().into())))
        .await
        .map_err(|_| "send timed out".to_string())?
        .map_err(|e| format!("ws send: {e}"))
}

/// Read frames until one with `op` arrives — stray heartbeats are
/// auto-acked — and return it.
async fn wait_for_frame(ws: &mut WebSocketStream<TcpStream>, op: u8) -> Result<Value, String> {
    let deadline = tokio::time::Instant::now() + FRAME_BUDGET;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let frame = tokio::time::timeout(remaining, ws.next())
            .await
            .map_err(|_| format!("timeout waiting for op {op}"))?
            .ok_or_else(|| "gateway connection closed".to_string())?
            .map_err(|e| format!("ws read: {e}"))?;
        let WsMessage::Text(text) = frame else { continue };
        let value: Value = serde_json::from_str(text.as_str()).map_err(|e| format!("bad frame: {e}"))?;
        if value["op"] == op {
            return Ok(value);
        }
        if value["op"] == 1 {
            send_frame(ws, &ack_frame()).await?;
        }
    }
}

/// Read one HTTP request (head + Content-Length body), bounded.
async fn read_request(sock: &mut TcpStream) -> Option<RecordedRequest> {
    let (head, body) = tokio::time::timeout(Duration::from_secs(10), async {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 8192];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = sock.read(&mut tmp).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        let split = buf.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
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
            let n = sock.read(&mut tmp).await.ok()?;
            if n == 0 {
                return None;
            }
            body.extend_from_slice(&tmp[..n]);
        }
        Some((head, String::from_utf8_lossy(&body).to_string()))
    })
    .await
    .ok()
    .flatten()?;
    let mut parts = head.split_whitespace();
    Some(RecordedRequest {
        method: parts.next().unwrap_or_default().to_string(),
        path: parts.next().unwrap_or_default().to_string(),
        body,
        head,
    })
}

/// A driver with an empty allowlist: every message is refused, so no task
/// ever starts — the refusal itself is still observable on the REST mock.
fn deny_all_driver(transport: Arc<dyn ChatTransport>) -> Arc<ChatDriver> {
    Arc::new(ChatDriver::new(
        Tenants::LegacyAllowlist(HashSet::new()),
        StubProvider::new(vec![]),
        PolicySource::Shared(Arc::new(AllowAllPolicyEngine)),
        registry_with_echo(ToolTrustTier::Observational),
        PathBuf::from("/tmp/amparo-chat-discord-test"),
        transport,
        Arc::new(ApprovalRouter::new()),
        true,
    ))
}

/// POSTs to one exact path from the recorded log.
fn posts_to(rest: &MockRest, path: &str) -> Vec<RecordedRequest> {
    rest.requests().into_iter().filter(|r| r.method == "POST" && r.path == path).collect()
}

/// The gateway protocol: identify (token + intents), heartbeat ack,
/// READY, MESSAGE_CREATE, RECONNECT — then a RESUME on the new connection
/// carrying the same session id and the last sequence.
#[tokio::test]
async fn gateway_identify_heartbeat_ready_and_resume_after_reconnect() {
    let rest = MockRest::start(false).await;
    let gw = MockGateway::start(
        vec![
            expect(2, |d| {
                check(d["token"] == "test-token", "IDENTIFY carried the wrong token")?;
                check(d["intents"] == 37376, "IDENTIFY carried the wrong intents")?;
                Ok(())
            }),
            expect(1, |d| {
                check(d.is_null() || d.is_number(), "heartbeat seq must be null or a number")?;
                Ok(())
            }),
            GwStep::Send(ack_frame()),
            GwStep::Send(json!({ "op": 0, "t": "READY", "s": 1, "d": { "session_id": "sess-1" } })),
            GwStep::Send(json!({
                "op": 0, "t": "MESSAGE_CREATE", "s": 2, "d": {
                    "id": "m1", "channel_id": "c1", "content": "hello",
                    "author": { "id": "222", "bot": false },
                    "member": { "user": { "id": "222", "bot": false } }
                }
            })),
            GwStep::Send(json!({ "op": 7, "d": null })),
            GwStep::Reconnect,
            expect(6, |d| {
                check(d["token"] == "test-token", "RESUME carried the wrong token")?;
                check(d["session_id"] == "sess-1", "RESUME carried the wrong session id")?;
                check(d["seq"] == 2, "RESUME carried the wrong sequence")?;
                Ok(())
            }),
        ],
        Arc::clone(&rest.log),
    )
    .await;

    let transport: Arc<dyn ChatTransport> =
        Arc::new(DiscordTransport::with_urls("test-token".into(), gw.ws_url(), rest.url()));
    tokio::spawn(transport.clone().receive(deny_all_driver(Arc::clone(&transport))));

    gw.finished().await.expect("gateway script");
    // The refused message (empty allowlist) arrived over the REST mock.
    wait_until(|| !rest.requests().is_empty()).await;
    let refusals = posts_to(&rest, "/channels/c1/messages");
    assert!(refusals.iter().any(|r| r.body.contains("not authorized")));
}

/// REST behavior: a 429 is slept out and retried once, approval messages
/// carry the Approve/Deny component pair, edits remove the buttons, and
/// long text is truncated to Discord's 2000-character limit.
#[tokio::test]
async fn rest_retries_429_and_carries_approval_components() {
    let rest = MockRest::start(true).await;
    let transport = Arc::new(DiscordTransport::with_urls(
        "test-token".into(),
        "ws://127.0.0.1:1".into(), // never contacted — pure REST test
        rest.url(),
    ));
    let chat = ChatRef { platform: "discord", chat_id: "c1".into(), user_id: "222".into() };
    let request = ApprovalRequest {
        call_id: "call_1".into(),
        tool_name: "run_command".into(),
        arguments: json!({ "command": "ls" }),
        reasons: vec!["external effector".into()],
        blast_radius: Some(BlastRadius::Network),
        session_label: None,
    };

    let msg = transport.send_approval(&chat, &request, "call_1").await.expect("send approval");
    assert_eq!(msg.chat_id, "c1");
    assert_eq!(msg.message_id, "mock_msg_1", "the retried request created the message");

    // The first POST was 429'd and retried exactly once: two recorded.
    let posts = posts_to(&rest, "/channels/c1/messages");
    assert_eq!(posts.len(), 2, "the 429 must be retried once");
    for post in &posts {
        assert!(
            post.head.to_lowercase().contains("authorization: bot test-token"),
            "message posts carry the bot Authorization header"
        );
        assert!(post.body.contains("\"approve:call_1\""), "body: {}", post.body);
        assert!(post.body.contains("\"deny:call_1\""), "body: {}", post.body);
        assert!(post.body.contains("\"style\":1"), "approve is a primary button");
        assert!(post.body.contains("\"style\":4"), "deny is a danger button");
    }

    transport.edit_approval(&msg, "Approved").await.expect("edit approval");
    let edits: Vec<_> = rest
        .requests()
        .into_iter()
        .filter(|r| r.method == "PATCH" && r.path == "/channels/c1/messages/mock_msg_1")
        .collect();
    assert_eq!(edits.len(), 1);
    assert!(edits[0].body.contains("\"components\":[]"), "buttons removed: {}", edits[0].body);
    assert!(edits[0].body.contains("Approved"));

    transport.send_text(&chat, &"x".repeat(2500)).await.expect("send text");
    let texts = posts_to(&rest, "/channels/c1/messages");
    let last = texts.last().expect("truncated text post");
    let parsed: Value = serde_json::from_str(&last.body).expect("json body");
    assert_eq!(
        parsed["content"].as_str().expect("content field").chars().count(),
        2000,
        "text is truncated to Discord's message limit"
    );
}

/// A full driver round trip through the real gateway path: a message from
/// an allowlisted user starts a task, the approval button press comes back
/// as an INTERACTION_CREATE, the interaction is acked (no Authorization),
/// the gate approves, and the final answer is delivered.
#[tokio::test]
async fn driver_runs_a_task_through_the_gateway_and_button_press() {
    let rest = MockRest::start(false).await;
    let gw = MockGateway::start(
        vec![
            expect(2, |d| {
                check(d["token"] == "test-token", "IDENTIFY carried the wrong token")?;
                check(d["intents"] == 37376, "IDENTIFY carried the wrong intents")?;
                Ok(())
            }),
            GwStep::Send(json!({ "op": 0, "t": "READY", "s": 1, "d": { "session_id": "sess-1" } })),
            GwStep::Send(json!({
                "op": 0, "t": "MESSAGE_CREATE", "s": 2, "d": {
                    "id": "m1", "channel_id": "c1", "content": "do the thing",
                    "author": { "id": "222", "bot": false },
                    "member": { "user": { "id": "222", "bot": false } }
                }
            })),
            wait_rest(|reqs| reqs.iter().any(|r| r.body.contains("approve:call_1"))),
            GwStep::Send(json!({
                "op": 0, "t": "INTERACTION_CREATE", "s": 3, "d": {
                    "id": "i1", "type": 3, "token": "tok123", "channel_id": "c1",
                    "data": { "custom_id": "approve:call_1", "component_type": 2 },
                    "message": { "id": "m1" },
                    "member": { "user": { "id": "222", "bot": false } }
                }
            })),
            wait_rest(|reqs| reqs.iter().any(|r| r.method == "POST" && r.body.contains("Done."))),
        ],
        Arc::clone(&rest.log),
    )
    .await;

    let transport: Arc<dyn ChatTransport> =
        Arc::new(DiscordTransport::with_urls("test-token".into(), gw.ws_url(), rest.url()));
    let driver = Arc::new(ChatDriver::new(
        Tenants::LegacyAllowlist(HashSet::from(["222".to_string()])),
        StubProvider::new(vec![
            turn_tool_call("call_1", "echo", r#"{"message":"hi"}"#),
            turn_text("Done."),
        ]),
        PolicySource::Shared(Arc::new(AllowAllPolicyEngine)),
        registry_with_echo(ToolTrustTier::ExternalEffector),
        std::env::temp_dir().join(format!("amparo-chat-discord-{}", std::process::id())),
        Arc::clone(&transport),
        Arc::new(ApprovalRouter::new()),
        false,
    ));
    tokio::spawn(transport.receive(driver));
    gw.finished().await.expect("gateway script");

    // The interaction was acked receiver-only — the callback URL carries
    // the token, and no Authorization header may go along.
    let callbacks = posts_to(&rest, "/interactions/i1/tok123/callback");
    assert_eq!(callbacks.len(), 1, "one interaction callback");
    assert!(
        !callbacks[0].head.to_lowercase().contains("authorization"),
        "callback must not carry an Authorization header"
    );
    assert!(callbacks[0].body.contains("\"type\":6"), "deferred update: {}", callbacks[0].body);

    // The gate approved and edited the message in place.
    let edits: Vec<_> = rest
        .requests()
        .into_iter()
        .filter(|r| r.method == "PATCH" && r.path.starts_with("/channels/c1/messages/"))
        .collect();
    assert_eq!(edits.len(), 1, "the gate is the message's single editor");
    assert!(edits[0].body.contains("\"Approved\""), "edit body: {}", edits[0].body);
    assert!(edits[0].body.contains("\"components\":[]"), "buttons removed");

    // The final answer was delivered as a text send.
    let posts = posts_to(&rest, "/channels/c1/messages");
    assert!(posts.iter().any(|r| r.body.contains("Done.")), "final answer delivered");
}

/// A press from a user who did not start the task is refused with a polite
/// toast — the requester's buttons stay — and the requester's own later
/// press still routes the decision.
#[tokio::test]
async fn wrong_user_press_gets_toast_and_requester_still_decides() {
    let rest = MockRest::start(false).await;
    let gw = MockGateway::start(
        vec![
            expect(2, |d| {
                check(d["token"] == "test-token", "IDENTIFY carried the wrong token")?;
                check(d["intents"] == 37376, "IDENTIFY carried the wrong intents")?;
                Ok(())
            }),
            GwStep::Send(json!({ "op": 0, "t": "READY", "s": 1, "d": { "session_id": "sess-1" } })),
            GwStep::Send(json!({
                "op": 0, "t": "MESSAGE_CREATE", "s": 2, "d": {
                    "id": "m1", "channel_id": "c1", "content": "do the thing",
                    "author": { "id": "222", "bot": false },
                    "member": { "user": { "id": "222", "bot": false } }
                }
            })),
            wait_rest(|reqs| reqs.iter().any(|r| r.body.contains("approve:call_1"))),
            GwStep::Send(json!({
                "op": 0, "t": "INTERACTION_CREATE", "s": 3, "d": {
                    "id": "i2", "type": 3, "token": "tok123", "channel_id": "c1",
                    "data": { "custom_id": "approve:call_1", "component_type": 2 },
                    "message": { "id": "m1" },
                    "member": { "user": { "id": "333", "bot": false } }
                }
            })),
            wait_rest(|reqs| {
                reqs.iter().any(|r| {
                    r.method == "POST"
                        && r.path == "/channels/c1/messages"
                        && r.body.contains("Only the user who started the task can decide.")
                })
            }),
            GwStep::Send(json!({
                "op": 0, "t": "INTERACTION_CREATE", "s": 4, "d": {
                    "id": "i3", "type": 3, "token": "tok123", "channel_id": "c1",
                    "data": { "custom_id": "approve:call_1", "component_type": 2 },
                    "message": { "id": "m1" },
                    "member": { "user": { "id": "222", "bot": false } }
                }
            })),
            wait_rest(|reqs| {
                reqs.iter().any(|r| r.method == "POST" && r.body.contains("Done."))
            }),
        ],
        Arc::clone(&rest.log),
    )
    .await;

    let transport: Arc<dyn ChatTransport> =
        Arc::new(DiscordTransport::with_urls("test-token".into(), gw.ws_url(), rest.url()));
    let driver = Arc::new(ChatDriver::new(
        Tenants::LegacyAllowlist(HashSet::from(["222".to_string()])),
        StubProvider::new(vec![
            turn_tool_call("call_1", "echo", r#"{"message":"hi"}"#),
            turn_text("Done."),
        ]),
        PolicySource::Shared(Arc::new(AllowAllPolicyEngine)),
        registry_with_echo(ToolTrustTier::ExternalEffector),
        std::env::temp_dir().join(format!("amparo-chat-discord-{}", std::process::id())),
        Arc::clone(&transport),
        Arc::new(ApprovalRouter::new()),
        false,
    ));
    tokio::spawn(transport.receive(driver));
    gw.finished().await.expect("gateway script");

    // Exactly one wrong-user toast, posted like any authorized message.
    let toasts: Vec<_> = posts_to(&rest, "/channels/c1/messages")
        .into_iter()
        .filter(|r| r.body.contains("Only the user who started the task can decide."))
        .collect();
    assert_eq!(toasts.len(), 1, "one wrong-user toast");
    assert!(
        toasts[0].head.to_lowercase().contains("authorization: bot test-token"),
        "the toast is an authorized message post"
    );

    // The requester's press routed: the gate edited the message and the
    // final answer was delivered.
    let edits: Vec<_> = rest
        .requests()
        .into_iter()
        .filter(|r| r.method == "PATCH" && r.path.starts_with("/channels/c1/messages/"))
        .collect();
    assert_eq!(edits.len(), 1, "the gate is the message's single editor");
    assert!(edits[0].body.contains("\"Approved\""), "edit body: {}", edits[0].body);
    let posts = posts_to(&rest, "/channels/c1/messages");
    assert!(posts.iter().any(|r| r.body.contains("Done.")), "final answer delivered");
}
