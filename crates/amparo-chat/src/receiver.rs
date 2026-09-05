//! The Telegram approval receiver (R3b) — the cross-surface handoff's
//! remote arm.
//!
//! The driver loop (`amparo chat telegram`) serves tasks; the receiver
//! (`amparo chat telegram --receiver URL`) relays approvals instead. It
//! polls the hub's pending listing (`GET URL`, the hub's `/api/approvals`
//! root), sends each pending request to every operator chat with the same
//! inline Approve/Deny buttons the driver loop uses, and posts each press
//! back to the hub (`POST URL/{call_id}/decide`). The hub's deny-wins
//! latch is the arbiter: the receiver never overrides it — a deny from
//! any surface dominates, and a 409 on a press means another surface
//! already decided (the toast says so).
//!
//! The receiver is a relay, not an agent: no inference, no policy, no
//! tool registry — it cannot execute anything, only forward decisions.
//! Credentials: the bot token stays `AMPARO_CHAT_TELEGRAM_TOKEN`, the hub
//! bearer is `AMPARO_APPROVAL_TOKEN` (the same approval-scoped token the
//! TUI fan-out gate uses), and the operators are the chats listed in
//! `AMPARO_CHAT_ALLOWLIST` — one id per private chat, absent or empty
//! refuses to start (fail closed). Messages the relay itself cannot send
//! are skipped after one attempt, never retried into a dead chat.

use crate::dispatch::{allowlist_from_env, ChatFlags, ChatServeError};
use crate::telegram::{telegram_base, TelegramTransport};
use crate::transport::{ApprovalMessage, ChatRef, ChatTransport};
use amparo_agent::ApprovalRequest;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often the hub's pending listing is polled. The hub auto-denies a
/// pending approval after 60 seconds, so 5 seconds leaves the operator
/// most of that budget to read and press.
const HUB_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How long one hub call may take — long enough for a slow round trip,
/// short enough that a wedged hub cannot stall the relay forever.
const HUB_HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// The hub client: the pending listing and the decision route, both
/// bearer-authed with the approval token. `root` is the operator-supplied
/// `/api/approvals` URL — `GET root` is the listing, `POST
/// root/{call_id}/decide` is a press.
struct HubClient {
    /// The listing URL, trailing slash trimmed — the decision route is
    /// `{root}/{call_id}/decide`.
    root: String,
    /// The approval-scoped bearer, sent on every call. Never logged.
    token: String,
    /// The HTTP client (bounded by [`HUB_HTTP_TIMEOUT`]).
    client: reqwest::Client,
}

/// The hub's answer to a decision POST.
enum DecideOutcome {
    /// The press decided the approval — this surface's decision is the
    /// one that latched.
    Decided(bool),
    /// Another surface decided first — the carried decision is the
    /// latched one (deny-wins was already applied by the hub).
    AlreadyDecided(bool),
    /// The hub has never heard of the call_id.
    Unknown,
}

impl HubClient {
    /// Build a client for the hub's `/api/approvals` root.
    fn new(root: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            root: root.into().trim_end_matches('/').to_string(),
            token: token.into(),
            client: reqwest::Client::builder()
                .timeout(HUB_HTTP_TIMEOUT)
                .build()
                .expect("static reqwest client options are valid"),
        }
    }

    /// GET the pending listing: every entry the hub knows about, pending
    /// and decided alike. Any failure is an error — the poller retries,
    /// and an approval that expires meanwhile auto-denies at the hub.
    async fn list(&self) -> Result<Vec<Value>, String> {
        let response = self
            .client
            .get(&self.root)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| format!("the hub is unreachable: {e}"))?;
        if response.status().as_u16() != 200 {
            return Err(format!(
                "listing failed: HTTP {} (check the URL and AMPARO_APPROVAL_TOKEN)",
                response.status()
            ));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|e| format!("listing was not JSON: {e}"))?;
        Ok(body
            .get("approvals")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// POST one press to the hub's decide route. The hub applies the
    /// deny-wins latch: `AlreadyDecided` carries the latched decision,
    /// so a late deny can still override an earlier approve (the hub's
    /// 200), and an approve over an earlier deny is a 409 with `false`.
    async fn decide(&self, call_id: &str, decision: bool) -> Result<DecideOutcome, String> {
        let url = format!("{}/{call_id}/decide", self.root);
        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(&json!({ "decision": decision }))
            .send()
            .await
            .map_err(|e| format!("the hub is unreachable: {e}"))?;
        let status = response.status().as_u16();
        let body: Value = response
            .json()
            .await
            .map_err(|e| format!("decision answer was not JSON: {e}"))?;
        let latched = body.get("decision").and_then(Value::as_bool);
        match status {
            200 => Ok(DecideOutcome::Decided(latched.unwrap_or(decision))),
            409 => Ok(DecideOutcome::AlreadyDecided(latched.unwrap_or(false))),
            404 => Ok(DecideOutcome::Unknown),
            other => Err(format!("decision route failed: HTTP {other}")),
        }
    }
}

/// The one-word outcome shown on settled approval messages.
fn outcome_text(decision: bool) -> &'static str {
    if decision {
        "Approved"
    } else {
        "Denied"
    }
}

/// Edit every tracked message for `call_id` to `text` (which clears the
/// buttons) and stop tracking it.
async fn settle(
    transport: &dyn ChatTransport,
    sent: &Mutex<HashMap<String, Vec<ApprovalMessage>>>,
    call_id: &str,
    text: &str,
) {
    let messages = sent.lock().unwrap().remove(call_id);
    let Some(messages) = messages else {
        return;
    };
    for msg in &messages {
        let _ = transport.edit_approval(msg, text).await;
    }
}

/// Reconcile one hub listing with what has been relayed: send every new
/// pending approval to every operator chat, and settle messages whose
/// approval another surface decided meanwhile. Entries a relay could not
/// send are remembered in `failed` — one attempt each, never retried
/// into a dead chat.
async fn sync(
    transport: &dyn ChatTransport,
    operators: &HashSet<String>,
    sent: &Mutex<HashMap<String, Vec<ApprovalMessage>>>,
    failed: &Mutex<HashSet<String>>,
    entries: &[Value],
) {
    for entry in entries {
        let Some(call_id) = entry.get("call_id").and_then(Value::as_str) else {
            continue;
        };
        let status = entry.get("status").and_then(Value::as_str);
        if status == Some("decided") {
            // The hub's deny-wins latch already picked the outcome; the
            // message (if we relayed it) is settled to match.
            let decision = entry.get("decision").and_then(Value::as_bool).unwrap_or(false);
            settle(transport, sent, call_id, outcome_text(decision)).await;
            continue;
        }
        if status != Some("pending") {
            continue;
        }
        let already = {
            let sent = sent.lock().unwrap();
            let failed = failed.lock().unwrap();
            sent.contains_key(call_id) || failed.contains(call_id)
        };
        if already {
            continue;
        }
        // Parse the listing entry back into the request so the relayed
        // copy is byte-identical to every other surface's.
        let Ok(request) = serde_json::from_value::<ApprovalRequest>(entry.clone()) else {
            eprintln!(
                "amparo chat receiver: undecodable approval entry for {call_id}; skipping"
            );
            failed.lock().unwrap().insert(call_id.to_string());
            continue;
        };
        let mut messages = Vec::new();
        let mut sent_all = true;
        for chat_id in operators {
            let chat = ChatRef {
                platform: "telegram",
                chat_id: chat_id.clone(),
                user_id: chat_id.clone(),
            };
            match transport.send_approval(&chat, &request, call_id).await {
                Ok(msg) => messages.push(msg),
                Err(error) => {
                    eprintln!(
                        "amparo chat receiver: sendMessage to {chat_id} failed: {error}; \
                         not retrying this approval"
                    );
                    sent_all = false;
                }
            }
        }
        if sent_all {
            sent.lock().unwrap().insert(call_id.to_string(), messages);
        } else {
            failed.lock().unwrap().insert(call_id.to_string());
        }
    }
}

/// The hub poller: list, reconcile, sleep — forever. The serve loop
/// aborts it on exit; a hub failure is logged and retried, never fatal.
async fn poll_hub(
    hub: Arc<HubClient>,
    transport: Arc<TelegramTransport>,
    operators: Arc<HashSet<String>>,
    sent: Arc<Mutex<HashMap<String, Vec<ApprovalMessage>>>>,
    failed: Arc<Mutex<HashSet<String>>>,
) {
    loop {
        match hub.list().await {
            Ok(entries) => {
                sync(transport.as_ref(), &operators, &sent, &failed, &entries).await;
            }
            Err(error) => {
                eprintln!("amparo chat receiver: hub listing failed: {error}");
            }
        }
        tokio::time::sleep(HUB_POLL_INTERVAL).await;
    }
}

/// How a button press is gated before it reaches the hub.
enum PressGate {
    /// An operator pressed one of our buttons: the call_id and decision.
    Decision(String, bool),
    /// The payload is not one of our buttons.
    NotAButton,
    /// A non-operator pressed — answered, never forwarded.
    WrongUser,
}

/// Gate one inline-button press: parse the payload and check the presser
/// against the operator list. Pure — the hub is never called for a press
/// that fails this gate.
fn gate_press(operators: &HashSet<String>, query: &crate::telegram::CallbackQuery) -> PressGate {
    let Some((call_id, decision)) = crate::telegram::parse_button(&query.data) else {
        return PressGate::NotAButton;
    };
    if !operators.contains(&query.from.id.to_string()) {
        return PressGate::WrongUser;
    }
    PressGate::Decision(call_id, decision)
}

/// Handle one inline-button press: operators only, decided against the
/// hub (deny-wins applies there), the pressed message settled, and the
/// client spinner always answered.
async fn handle_press(
    transport: &TelegramTransport,
    hub: &HubClient,
    operators: &HashSet<String>,
    sent: &Mutex<HashMap<String, Vec<ApprovalMessage>>>,
    query: &crate::telegram::CallbackQuery,
) {
    let (call_id, decision) = match gate_press(operators, query) {
        PressGate::Decision(call_id, decision) => (call_id, decision),
        PressGate::NotAButton => {
            let _ = transport.answer_callback(&query.id, "Unknown action").await;
            return;
        }
        PressGate::WrongUser => {
            let _ = transport
                .answer_callback(&query.id, crate::driver::WRONG_USER_TOAST)
                .await;
            return;
        }
    };
    let toast = match hub.decide(&call_id, decision).await {
        Ok(DecideOutcome::Decided(latched)) => {
            settle(transport, sent, &call_id, outcome_text(latched)).await;
            outcome_text(latched)
        }
        Ok(DecideOutcome::AlreadyDecided(latched)) => {
            let text = format!("Already decided — {}", outcome_text(latched));
            settle(transport, sent, &call_id, &text).await;
            "Already decided — no longer pending"
        }
        Ok(DecideOutcome::Unknown) => "Unknown approval",
        Err(error) => {
            eprintln!("amparo chat receiver: decision POST failed: {error}");
            "Hub unreachable — try again"
        }
    };
    let _ = transport.answer_callback(&query.id, toast).await;
}

/// Serve the Telegram receiver: bot token, approval token and operators
/// fail closed (exit 2), then the hub poller and the `getUpdates` pump
/// until Ctrl-C or a fatal token rejection (exit 1). No inference or
/// policy wiring exists here — the relay cannot execute anything.
pub async fn serve(flags: &ChatFlags) -> Result<(), ChatServeError> {
    let bot_token = std::env::var("AMPARO_CHAT_TELEGRAM_TOKEN").map_err(|_| {
        ChatServeError::new(
            "AMPARO_CHAT_TELEGRAM_TOKEN is required — see the README chat section",
            2,
        )
    })?;
    let approval_token = std::env::var("AMPARO_APPROVAL_TOKEN").map_err(|_| {
        ChatServeError::new(
            "AMPARO_APPROVAL_TOKEN is required in receiver mode — the hub's \
             approval-scoped bearer (the same token the TUI fan-out gate uses)",
            2,
        )
    })?;
    if flags.chat_config.is_some() {
        return Err(ChatServeError::new(
            "--chat-config does not apply in receiver mode — the operators are the chats \
             in AMPARO_CHAT_ALLOWLIST",
            2,
        ));
    }
    let operators = allowlist_from_env();
    if operators.is_empty() {
        return Err(ChatServeError::new(
            "receiver mode needs at least one operator chat id in AMPARO_CHAT_ALLOWLIST",
            2,
        ));
    }
    let Some(root) = &flags.receiver else {
        // dispatch() only routes here with --receiver set — an internal
        // invariant, not an operator error, but fail closed regardless.
        return Err(ChatServeError::new(
            "receiver mode started without a hub URL",
            2,
        ));
    };

    let transport = Arc::new(TelegramTransport::new(telegram_base(), bot_token));
    let hub = Arc::new(HubClient::new(root.clone(), approval_token));
    let operators: Arc<HashSet<String>> = Arc::new(operators);
    let sent: Arc<Mutex<HashMap<String, Vec<ApprovalMessage>>>> = Arc::default();
    let failed: Arc<Mutex<HashSet<String>>> = Arc::default();
    eprintln!(
        "[receiver] relaying pending approvals from {root} to {} operator chat(s)",
        operators.len()
    );

    let poller = tokio::spawn(poll_hub(
        Arc::clone(&hub),
        Arc::clone(&transport),
        Arc::clone(&operators),
        Arc::clone(&sent),
        Arc::clone(&failed),
    ));

    let pump_transport = Arc::clone(&transport);
    let result = transport.pump(move |update| {
        let transport = Arc::clone(&pump_transport);
        let hub = Arc::clone(&hub);
        let operators = Arc::clone(&operators);
        let sent = Arc::clone(&sent);
        async move {
            if let Some(query) = &update.callback_query {
                handle_press(&transport, &hub, &operators, &sent, query).await;
            }
        }
    });

    let result = tokio::select! {
        result = result => result,
        _ = tokio::signal::ctrl_c() => Ok(()),
    };
    poller.abort();
    result.map_err(|error| ChatServeError::new(error.to_string(), 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telegram::parse_button;
    use crate::transport::ChatError;
    use async_trait::async_trait;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// One connection = one request/response (the MockPolicy shape). The
    /// handler receives the full request head (so tests can read the
    /// authorization header) and the body, and answers `(status, body)`.
    struct Responder {
        addr: std::net::SocketAddr,
    }

    impl Responder {
        async fn start(
            handler: impl Fn(String, String) -> (u16, String) + Send + Sync + 'static,
        ) -> Responder {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind mock hub");
            let addr = listener.local_addr().unwrap();
            let handler: Arc<dyn Fn(String, String) -> (u16, String) + Send + Sync> =
                Arc::new(handler);
            tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        break;
                    };
                    let handler = Arc::clone(&handler);
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
                        // A connection that delivered nothing (the client
                        // opened and abandoned it) is not a request — a
                        // real server routes nothing for it, and
                        // dispatching would flake the per-request asserts.
                        if buf.is_empty() {
                            return;
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
                        let (status, body) =
                            handler(head, String::from_utf8_lossy(&body).to_string());
                        let resp = format!(
                            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                    });
                }
            });
            Responder { addr }
        }

        fn url(&self) -> String {
            format!("http://{}/api/approvals", self.addr)
        }
    }

    /// A pending listing entry in the hub's view shape — the fields the
    /// server stores, plus the extras (timestamps, status, decision) the
    /// relay must ignore when parsing.
    fn pending_entry(call_id: &str) -> Value {
        json!({
            "call_id": call_id,
            "tool_name": "run_command",
            "arguments": {"command": "git push origin main"},
            "reasons": ["policy escalated this call for human review"],
            "blast_radius": "destructive",
            "session_label": "sub-agent sess-123.1 of task sess-123",
            "receivedAt": 1,
            "expiresAt": 2,
            "status": "pending",
            "decision": null,
            "auto": false,
        })
    }

    fn decided_entry(call_id: &str, decision: bool) -> Value {
        let mut entry = pending_entry(call_id);
        entry["status"] = "decided".into();
        entry["decision"] = decision.into();
        entry
    }

    fn listing(entries: Vec<Value>) -> String {
        json!({ "approvals": entries }).to_string()
    }

    /// An in-test transport recording the outbound calls.
    struct MockTransport {
        approvals: Mutex<Vec<(String, String, String)>>, // chat_id, approval_id, text
        edits: Mutex<Vec<(String, String)>>,             // message_id, outcome
    }

    #[async_trait]
    impl ChatTransport for MockTransport {
        async fn send_text(&self, _chat: &ChatRef, _text: &str) -> Result<(), ChatError> {
            Ok(())
        }

        async fn send_approval(
            &self,
            chat: &ChatRef,
            request: &ApprovalRequest,
            approval_id: &str,
        ) -> Result<ApprovalMessage, ChatError> {
            let text = format!(
                "{}: {}",
                request.tool_name,
                request.arguments
            );
            self.approvals.lock().unwrap().push((
                chat.chat_id.clone(),
                approval_id.to_string(),
                text,
            ));
            Ok(ApprovalMessage {
                chat_id: chat.chat_id.clone(),
                message_id: format!("msg-{}-{}", chat.chat_id, approval_id),
            })
        }

        async fn edit_approval(
            &self,
            msg: &ApprovalMessage,
            outcome: &str,
        ) -> Result<(), ChatError> {
            self.edits
                .lock()
                .unwrap()
                .push((msg.message_id.clone(), outcome.to_string()));
            Ok(())
        }

        async fn receive(self: Arc<Self>, _driver: Arc<crate::driver::ChatDriver>) -> Result<(), ChatError> {
            Ok(())
        }
    }

    fn operators() -> HashSet<String> {
        HashSet::from(["111".to_string(), "222".to_string()])
    }

    #[tokio::test]
    async fn list_sends_the_bearer_and_parses_entries() {
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let rec = Arc::clone(&seen);
        let server = Responder::start(move |head, _body| {
            rec.lock().unwrap().push(head);
            (200, listing(vec![pending_entry("call_1")]))
        })
        .await;
        let hub = HubClient::new(server.url(), "drill-token");
        let entries = hub.list().await.expect("listing succeeds");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["call_id"], "call_1");
        let head = seen.lock().unwrap();
        assert_eq!(head.len(), 1, "one request");
        let path = reqwest::Url::parse(&server.url()).unwrap().path().to_string();
        assert!(
            head[0].starts_with(&format!("GET {path} HTTP")),
            "the listing is a GET of the root: {}",
            head[0].lines().next().unwrap_or_default()
        );
        assert!(
            head[0]
                .to_ascii_lowercase()
                .contains("authorization: bearer drill-token"),
            "the approval token rides as a bearer: {}",
            head[0].lines().find(|l| l.to_ascii_lowercase().starts_with("authorization")).unwrap_or_default()
        );
    }

    #[tokio::test]
    async fn list_failure_is_an_error() {
        let server = Responder::start(|_head, _body| (401, "{}".to_string())).await;
        let hub = HubClient::new(server.url(), "drill-token");
        let error = hub.list().await.expect_err("a 401 is an error");
        assert!(
            error.contains("HTTP 401"),
            "the error names the status: {error}"
        );
    }

    #[tokio::test]
    async fn decide_routes_the_press_and_reads_the_latch() {
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let rec = Arc::clone(&seen);
        let server = Responder::start(move |head, body| {
            let is_post = head.starts_with("POST");
            rec.lock().unwrap().push(head);
            if is_post {
                assert_eq!(body, json!({"decision": true}).to_string());
                (200, json!({"call_id": "call_1", "status": "decided", "decision": true}).to_string())
            } else {
                (404, "{}".to_string())
            }
        })
        .await;
        let hub = HubClient::new(server.url(), "drill-token");
        assert!(matches!(
            hub.decide("call_1", true).await,
            Ok(DecideOutcome::Decided(true))
        ));
        let head = seen.lock().unwrap();
        let path = reqwest::Url::parse(&server.url()).unwrap().path().to_string();
        assert!(
            head[0].starts_with(&format!("POST {path}/call_1/decide HTTP")),
            "the press POSTs the decide route: {}",
            head[0].lines().next().unwrap_or_default()
        );
        assert!(
            head[0]
                .to_ascii_lowercase()
                .contains("authorization: bearer drill-token"),
            "the decide route carries the bearer too"
        );
    }

    #[tokio::test]
    async fn decide_maps_conflict_and_unknown() {
        let server = Responder::start(|head, _body| {
            if head.contains("/decide") {
                (409, json!({"error": "already decided", "decision": false, "auto": false}).to_string())
            } else {
                (404, "{}".to_string())
            }
        })
        .await;
        let hub = HubClient::new(server.url(), "drill-token");
        assert!(matches!(
            hub.decide("call_1", true).await,
            Ok(DecideOutcome::AlreadyDecided(false))
        ));
        let server = Responder::start(|head, _body| {
            if head.contains("/decide") {
                (404, "{}".to_string())
            } else {
                (404, "{}".to_string())
            }
        })
        .await;
        let hub = HubClient::new(server.url(), "drill-token");
        assert!(matches!(
            hub.decide("ghost", true).await,
            Ok(DecideOutcome::Unknown)
        ));
    }

    #[tokio::test]
    async fn sync_sends_new_pending_to_every_operator_once() {
        let transport = Arc::new(MockTransport {
            approvals: Mutex::new(Vec::new()),
            edits: Mutex::new(Vec::new()),
        });
        let sent: Mutex<HashMap<String, Vec<ApprovalMessage>>> = Mutex::new(HashMap::new());
        let failed: Mutex<HashSet<String>> = Mutex::new(HashSet::new());
        let entries = vec![pending_entry("call_1"), pending_entry("call_2")];
        sync(transport.as_ref(), &operators(), &sent, &failed, &entries).await;
        let approvals = transport.approvals.lock().unwrap();
        assert_eq!(approvals.len(), 4, "two approvals, two operator chats");
        let call_1_chats: HashSet<&str> = approvals
            .iter()
            .filter(|(_, id, _)| id == "call_1")
            .map(|(chat, _, _)| chat.as_str())
            .collect();
        assert_eq!(
            call_1_chats,
            HashSet::from(["111", "222"]),
            "every operator gets every approval"
        );
        assert!(
            approvals[0].2.contains("git push origin main"),
            "the relayed copy carries the arguments: {}",
            approvals[0].2
        );
        // Release the guard before the await below — an async fn drops
        // bindings at scope end, and the final assert re-locks this same
        // mutex (a held-across-await guard would self-deadlock).
        drop(approvals);
        assert_eq!(
            sent.lock().unwrap().len(),
            2,
            "both approvals are tracked for settling"
        );
        // A second sync of the same listing sends nothing new.
        sync(transport.as_ref(), &operators(), &sent, &failed, &entries).await;
        assert_eq!(
            transport.approvals.lock().unwrap().len(),
            4,
            "pending approvals are relayed once"
        );
    }

    #[tokio::test]
    async fn sync_settles_decided_approvals_and_skips_undecodable_entries() {
        let transport = Arc::new(MockTransport {
            approvals: Mutex::new(Vec::new()),
            edits: Mutex::new(Vec::new()),
        });
        let sent: Mutex<HashMap<String, Vec<ApprovalMessage>>> = Mutex::new(HashMap::new());
        let failed: Mutex<HashSet<String>> = Mutex::new(HashSet::new());
        // One pending that is relayed, then decided by another surface
        // (the hub's listing carries the latched decision).
        sync(
            transport.as_ref(),
            &operators(),
            &sent,
            &failed,
            &[pending_entry("call_1")],
        )
        .await;
        sync(
            transport.as_ref(),
            &operators(),
            &sent,
            &failed,
            &[decided_entry("call_1", true)],
        )
        .await;
        let edits = transport.edits.lock().unwrap();
        assert_eq!(edits.len(), 2, "both operator messages are settled");
        assert!(
            edits.iter().all(|(_, outcome)| outcome == "Approved"),
            "the settle text is the latched outcome: {edits:?}"
        );
        drop(edits);
        assert!(
            sent.lock().unwrap().is_empty(),
            "settled approvals stop being tracked"
        );

        // An entry missing its tool_name cannot be parsed — skipped and
        // remembered, not retried into a chat.
        let mut broken = pending_entry("call_2");
        broken.as_object_mut().unwrap().remove("tool_name");
        sync(transport.as_ref(), &operators(), &sent, &failed, &[broken]).await;
        assert_eq!(
            transport.approvals.lock().unwrap().len(),
            2,
            "the broken entry sends nothing"
        );
        assert!(
            failed.lock().unwrap().contains("call_2"),
            "broken entries are skipped, not retried"
        );
    }

    #[tokio::test]
    async fn listing_entries_parse_into_requests_ignoring_hub_extras() {
        // The hub view adds timestamps/status/decision/auto and never
        // carries the rollback field — the parse must ignore the former
        // and default the latter.
        let request: ApprovalRequest =
            serde_json::from_value(pending_entry("call_1")).expect("hub entry parses");
        assert_eq!(request.call_id, "call_1");
        assert_eq!(request.tool_name, "run_command");
        assert_eq!(request.arguments["command"], "git push origin main");
        assert_eq!(request.reasons.len(), 1);
        assert_eq!(
            request.blast_radius,
            Some(amparo_agent::BlastRadius::Destructive)
        );
        assert_eq!(request.session_label.as_deref(), Some("sub-agent sess-123.1 of task sess-123"));
        assert!(request.rollback.is_none(), "the hub view carries no rollback");
    }

    #[test]
    fn press_gate_accepts_operators_and_refuses_everyone_else() {
        let operators = operators();
        let press = |user: i64, data: Option<&str>| crate::telegram::CallbackQuery {
            id: "cb-1".to_string(),
            from: crate::telegram::User { id: user },
            message: None,
            data: data.map(str::to_string),
        };
        assert!(matches!(
            gate_press(&operators, &press(111, Some("approve:call_1"))),
            PressGate::Decision(id, true) if id == "call_1"
        ));
        assert!(matches!(
            gate_press(&operators, &press(222, Some("deny:call_9"))),
            PressGate::Decision(id, false) if id == "call_9"
        ));
        // A non-operator press is answered and never reaches the hub.
        assert!(matches!(
            gate_press(&operators, &press(999, Some("approve:call_1"))),
            PressGate::WrongUser
        ));
        assert!(matches!(
            gate_press(&operators, &press(111, Some("nonsense"))),
            PressGate::NotAButton
        ));
        assert!(matches!(
            gate_press(&operators, &press(111, None)),
            PressGate::NotAButton
        ));
    }

    #[test]
    fn button_payloads_are_the_hub_call_ids() {
        // The relay reuses the driver's payload grammar — the call_id is
        // the hub key, so a press lands on POST /api/approvals/{id}/decide.
        assert_eq!(
            parse_button(&Some("approve:call_1".to_string())),
            Some(("call_1".to_string(), true))
        );
        assert_eq!(
            parse_button(&Some("deny:call_1".to_string())),
            Some(("call_1".to_string(), false))
        );
    }
}
